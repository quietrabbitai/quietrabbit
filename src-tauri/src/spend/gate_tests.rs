// src-tauri/src/spend/gate_tests.rs
//
// Tests for the spend gate (items.id=710). Neutral fixtures only: no provider
// or model is named, no network is touched.

use chrono::{Duration, FixedOffset, TimeZone, Utc};

use super::*;
use crate::persistence::provider_spend_store::{ConsentRow, LimitSpec, UsageTotals};
use crate::providers::hosted_client::{HostedUsage, RateLimitInfo};
use crate::providers::types::{CompletionStatus, GenerateResponse};
use crate::spend::disclosure::attributes_hash;
use crate::spend::fixtures::{model, provider};
use crate::spend::price::parse_pricing;

const TEST_KEY_HEX: &str = "0202020202020202020202020202020202020202020202020202020202020202";

// $0.50 per million in, $1.50 per million out.
const PRICE_TEXT: &str =
    r#"{"input_per_mtok":0.50,"output_per_mtok":1.50,"as_of":"2026-10-11","source":"curated"}"#;
const FREE_TEXT: &str =
    r#"{"input_per_mtok":0,"output_per_mtok":0,"as_of":"2026-10-11","source":"curated"}"#;

fn known(text: &str) -> PriceQuote {
    PriceQuote::Known(parse_pricing(text, None).expect("fixture price"))
}

fn est(quote: &PriceQuote, input_tokens: u64, output_limit: u64) -> Estimate {
    match quote {
        PriceQuote::Known(p) => worst_case(p, input_tokens, output_limit),
        PriceQuote::Unknown => panic!("fixture price must be known"),
    }
}

fn consent(hash: &str, no_cap: bool) -> ConsentStanding {
    ConsentStanding {
        current: Some(ConsentRow {
            id: "c1".to_owned(),
            provider_id: "fixture-host-a".to_owned(),
            attributes_hash: hash.to_owned(),
            consented_at: "t".to_owned(),
            revoked_at: None,
            no_cap_confirmed: no_cap,
        }),
        any_row: true,
    }
}

#[allow(clippy::too_many_arguments)]
fn limit(
    id: &str,
    metric: LimitMetric,
    period: LimitPeriod,
    scope: ScopeKind,
    scope_id: Option<&str>,
    value: i64,
    approves: bool,
    spent: UsageTotals,
) -> LimitFact {
    LimitFact {
        row: LimitRow {
            id: id.to_owned(),
            spec: LimitSpec {
                metric,
                period,
                scope_kind: scope,
                scope_id: scope_id.map(str::to_owned),
                limit_value: value,
                warn_at_pct: 80,
                approves_unattended: approves,
            },
            created_at: "t".to_owned(),
            updated_at: "t".to_owned(),
        },
        spent,
    }
}

fn month_cap(value: i64, spent_micros: i64) -> LimitFact {
    limit(
        "cap",
        LimitMetric::CostUsd,
        LimitPeriod::Month,
        ScopeKind::All,
        None,
        value,
        false,
        UsageTotals {
            cost_micros: spent_micros,
            ..Default::default()
        },
    )
}

fn decide(
    standing: &ConsentStanding,
    quote: &PriceQuote,
    estimate: Option<&Estimate>,
    limits: &[LimitFact],
    attendance: RunAttendance,
) -> Result<Vec<LimitNotice>, SpendBlock> {
    evaluate(&Facts {
        standing,
        current_hash: "H",
        quote,
        estimate,
        limits,
        attendance,
    })
}

fn paid() -> (PriceQuote, Estimate) {
    let q = known(PRICE_TEXT);
    // 1000 in x 500_000 = 500 micro, 1000 out x 1_500_000 = 1500 micro: 2000.
    let e = est(&q, 1000, 1000);
    assert_eq!(e.worst_case_micros, 2000);
    (q, e)
}

fn code_of(r: Result<Vec<LimitNotice>, SpendBlock>) -> &'static str {
    match r {
        Ok(_) => "ok",
        Err(b) => b.code_str(),
    }
}

// ---------------------------------------------------------------------------
// Attendance default
// ---------------------------------------------------------------------------

#[test]
fn attendance_defaults_to_unattended() {
    assert_eq!(RunAttendance::default(), RunAttendance::Unattended);
}

// ---------------------------------------------------------------------------
// Consent and price
// ---------------------------------------------------------------------------

#[test]
fn no_consent_blocks_before_anything_else() {
    let (q, e) = paid();
    let none = ConsentStanding {
        current: None,
        any_row: false,
    };
    let r = decide(&none, &q, Some(&e), &[], RunAttendance::Attended);
    let b = r.unwrap_err();
    assert_eq!(b.code, Code::BlockConsentRequired);
    assert_eq!(b.params["state"], "none");

    let revoked = ConsentStanding {
        current: None,
        any_row: true,
    };
    let b = decide(&revoked, &q, Some(&e), &[], RunAttendance::Attended).unwrap_err();
    assert_eq!(b.params["state"], "revoked");
}

#[test]
fn changed_attributes_require_reconsent() {
    let (q, e) = paid();
    let old = consent("OLD", false);
    let r = decide(
        &old,
        &q,
        Some(&e),
        &[month_cap(10_000, 0)],
        RunAttendance::Attended,
    );
    assert_eq!(code_of(r), "spend_block.reconsent_required");
}

#[test]
fn an_unknown_price_stops_and_asks_never_passes() {
    let c = consent("H", true);
    let r = decide(&c, &PriceQuote::Unknown, None, &[], RunAttendance::Attended);
    assert_eq!(code_of(r), "spend_block.price_unknown");
    // Even attended, even with a huge cap: an uncappable call does not pass.
    let r = decide(
        &c,
        &PriceQuote::Unknown,
        None,
        &[month_cap(i64::MAX, 0)],
        RunAttendance::Attended,
    );
    assert_eq!(code_of(r), "spend_block.price_unknown");
}

#[test]
fn consent_is_checked_before_price() {
    let none = ConsentStanding {
        current: None,
        any_row: false,
    };
    let r = decide(
        &none,
        &PriceQuote::Unknown,
        None,
        &[],
        RunAttendance::Attended,
    );
    assert_eq!(code_of(r), "spend_block.consent_required");
}

// ---------------------------------------------------------------------------
// Cap rule
// ---------------------------------------------------------------------------

#[test]
fn a_call_that_can_cost_money_needs_a_cap_or_a_confirmed_no_cap() {
    let (q, e) = paid();
    let c = consent("H", false);
    let r = decide(&c, &q, Some(&e), &[], RunAttendance::Attended);
    let b = r.unwrap_err();
    assert_eq!(b.code, Code::BlockCapRequired);
    assert_eq!(b.params["estimate_micros"], 2000);

    // A day, week or month cost limit is a cap.
    for period in [LimitPeriod::Day, LimitPeriod::Week, LimitPeriod::Month] {
        let cap = limit(
            "cap",
            LimitMetric::CostUsd,
            period,
            ScopeKind::Provider,
            Some("fixture-host-a"),
            1_000_000,
            false,
            UsageTotals::default(),
        );
        assert!(decide(&c, &q, Some(&e), &[cap], RunAttendance::Attended).is_ok());
    }

    // A per-run limit bounds one run, not cumulative spend: not a cap.
    let run_only = limit(
        "r",
        LimitMetric::CostUsd,
        LimitPeriod::Run,
        ScopeKind::All,
        None,
        1_000_000,
        false,
        UsageTotals::default(),
    );
    assert_eq!(
        code_of(decide(
            &c,
            &q,
            Some(&e),
            &[run_only],
            RunAttendance::Attended
        )),
        "spend_block.cap_required"
    );
    // Neither is a token limit.
    let tokens = limit(
        "t",
        LimitMetric::TokensOut,
        LimitPeriod::Month,
        ScopeKind::All,
        None,
        1_000_000,
        false,
        UsageTotals::default(),
    );
    assert_eq!(
        code_of(decide(&c, &q, Some(&e), &[tokens], RunAttendance::Attended)),
        "spend_block.cap_required"
    );

    // An explicit no-cap confirmation on the consent satisfies the rule.
    let confirmed = consent("H", true);
    assert!(decide(&confirmed, &q, Some(&e), &[], RunAttendance::Attended).is_ok());
}

#[test]
fn a_free_call_needs_no_cap() {
    let q = known(FREE_TEXT);
    let e = est(&q, 5000, 4096);
    assert_eq!(e.worst_case_micros, 0);
    let c = consent("H", false);
    assert!(decide(&c, &q, Some(&e), &[], RunAttendance::Attended).is_ok());
}

// ---------------------------------------------------------------------------
// Limits
// ---------------------------------------------------------------------------

#[test]
fn a_limit_blocks_only_when_the_projection_exceeds_it() {
    let (q, e) = paid();
    let c = consent("H", true);
    // 1000 spent + 2000 projected = 3000: exactly at the limit passes.
    assert!(decide(
        &c,
        &q,
        Some(&e),
        &[month_cap(3000, 1000)],
        RunAttendance::Attended
    )
    .is_ok());
    // One micro over blocks.
    let b = decide(
        &c,
        &q,
        Some(&e),
        &[month_cap(2999, 1000)],
        RunAttendance::Attended,
    )
    .unwrap_err();
    assert_eq!(b.code, Code::BlockLimitReached);
    assert_eq!(b.params["metric"], "cost_usd");
    assert_eq!(b.params["period"], "month");
    assert_eq!(b.params["scope_kind"], "all");
    assert_eq!(b.params["spent"], 1000);
    assert_eq!(b.params["limit"], 2999);
    assert_eq!(b.params["projected_add"], 2000);
}

#[test]
fn each_metric_projects_its_own_quantity() {
    let (q, e) = paid();
    let c = consent("H", true);
    let t = |used: UsageTotals| used;
    let in_limit = |value: i64, used: UsageTotals| {
        limit(
            "l",
            LimitMetric::TokensIn,
            LimitPeriod::Day,
            ScopeKind::All,
            None,
            value,
            false,
            t(used),
        )
    };
    let out_limit = |value: i64, used: UsageTotals| {
        limit(
            "l",
            LimitMetric::TokensOut,
            LimitPeriod::Day,
            ScopeKind::All,
            None,
            value,
            false,
            t(used),
        )
    };
    let req_limit = |value: i64, used: UsageTotals| {
        limit(
            "l",
            LimitMetric::Requests,
            LimitPeriod::Day,
            ScopeKind::All,
            None,
            value,
            false,
            t(used),
        )
    };
    // Input estimate is 1000 tokens, output limit 1000 tokens, one request.
    let used = UsageTotals {
        tokens_in: 500,
        tokens_out: 500,
        requests: 4,
        cost_micros: 0,
    };
    assert!(decide(
        &c,
        &q,
        Some(&e),
        &[in_limit(1500, used)],
        RunAttendance::Attended
    )
    .is_ok());
    assert!(decide(
        &c,
        &q,
        Some(&e),
        &[in_limit(1499, used)],
        RunAttendance::Attended
    )
    .is_err());
    assert!(decide(
        &c,
        &q,
        Some(&e),
        &[out_limit(1500, used)],
        RunAttendance::Attended
    )
    .is_ok());
    assert!(decide(
        &c,
        &q,
        Some(&e),
        &[out_limit(1499, used)],
        RunAttendance::Attended
    )
    .is_err());
    assert!(decide(
        &c,
        &q,
        Some(&e),
        &[req_limit(5, used)],
        RunAttendance::Attended
    )
    .is_ok());
    assert!(decide(
        &c,
        &q,
        Some(&e),
        &[req_limit(4, used)],
        RunAttendance::Attended
    )
    .is_err());
}

#[test]
fn limits_apply_even_to_a_free_call() {
    let q = known(FREE_TEXT);
    let e = est(&q, 1000, 1000);
    let c = consent("H", false);
    let req_limit = limit(
        "l",
        LimitMetric::Requests,
        LimitPeriod::Day,
        ScopeKind::All,
        None,
        3,
        false,
        UsageTotals {
            requests: 3,
            ..Default::default()
        },
    );
    assert_eq!(
        code_of(decide(
            &c,
            &q,
            Some(&e),
            &[req_limit],
            RunAttendance::Attended
        )),
        "spend_block.limit_reached"
    );
}

#[test]
fn reaching_the_warn_threshold_proceeds_and_is_reported() {
    let (q, e) = paid();
    let c = consent("H", true);
    // 80 percent of 10_000 is 8000: 6000 spent + 2000 projected is exactly it.
    let notices = decide(
        &c,
        &q,
        Some(&e),
        &[month_cap(10_000, 6000)],
        RunAttendance::Attended,
    )
    .expect("a warning does not block");
    assert_eq!(notices.len(), 1);
    assert_eq!(notices[0].spent, 6000);
    assert_eq!(notices[0].projected_add, 2000);
    // One micro under the threshold: no notice.
    let quiet = decide(
        &c,
        &q,
        Some(&e),
        &[month_cap(10_000, 5999)],
        RunAttendance::Attended,
    )
    .unwrap();
    assert!(quiet.is_empty());
}

#[test]
fn the_most_restrictive_applicable_limit_governs() {
    let (q, e) = paid();
    let c = consent("H", true);
    let loose = month_cap(1_000_000, 0);
    let tight = limit(
        "tight",
        LimitMetric::CostUsd,
        LimitPeriod::Day,
        ScopeKind::Persona,
        Some("persona-1"),
        1500,
        false,
        UsageTotals::default(),
    );
    let b = decide(&c, &q, Some(&e), &[loose, tight], RunAttendance::Attended).unwrap_err();
    assert_eq!(b.params["limit_id"], "tight");
}

#[test]
fn projection_saturates_instead_of_overflowing() {
    let (q, e) = paid();
    let c = consent("H", true);
    let huge = month_cap(i64::MAX, i64::MAX);
    assert_eq!(
        code_of(decide(&c, &q, Some(&e), &[huge], RunAttendance::Attended)),
        "spend_block.limit_reached"
    );
}

// ---------------------------------------------------------------------------
// Unattended runs
// ---------------------------------------------------------------------------

fn run_budget(
    id: &str,
    scope: ScopeKind,
    scope_id: Option<&str>,
    value: i64,
    approves: bool,
    spent: i64,
) -> LimitFact {
    limit(
        id,
        LimitMetric::CostUsd,
        LimitPeriod::Run,
        scope,
        scope_id,
        value,
        approves,
        UsageTotals {
            cost_micros: spent,
            ..Default::default()
        },
    )
}

#[test]
fn an_unattended_run_without_an_approved_budget_parks() {
    let (q, e) = paid();
    let c = consent("H", true);
    let b = decide(&c, &q, Some(&e), &[], RunAttendance::Unattended).unwrap_err();
    assert_eq!(b.code, Code::BlockBudgetApprovalRequired);
    assert_eq!(b.params["estimate_micros"], 2000);
    assert_eq!(b.params["missing_approval"], "per_run_cost_limit");

    // A per-run limit that is NOT marked approves nothing.
    let unmarked = run_budget("r", ScopeKind::All, None, 1_000_000, false, 0);
    assert_eq!(
        code_of(decide(
            &c,
            &q,
            Some(&e),
            &[unmarked],
            RunAttendance::Unattended
        )),
        "spend_block.budget_approval_required"
    );
    // Neither does a marked monthly cap (the schema forbids the flag there,
    // and the gate does not trust it anyway).
    let mut monthly = month_cap(1_000_000, 0);
    monthly.row.spec.approves_unattended = true;
    assert_eq!(
        code_of(decide(
            &c,
            &q,
            Some(&e),
            &[monthly],
            RunAttendance::Unattended
        )),
        "spend_block.budget_approval_required"
    );
}

#[test]
fn an_unattended_run_spends_within_an_approved_budget() {
    let (q, e) = paid();
    let c = consent("H", true);
    let approved = run_budget("r", ScopeKind::All, None, 5000, true, 0);
    assert!(decide(&c, &q, Some(&e), &[approved], RunAttendance::Unattended).is_ok());
}

#[test]
fn an_approved_budget_is_still_a_ceiling() {
    let (q, e) = paid();
    let c = consent("H", true);
    // 4000 already spent in this run + 2000 projected > 5000.
    let approved = run_budget("r", ScopeKind::All, None, 5000, true, 4000);
    let b = decide(&c, &q, Some(&e), &[approved], RunAttendance::Unattended).unwrap_err();
    assert_eq!(b.code, Code::BlockLimitReached);
    assert_eq!(b.params["period"], "run");
    assert_eq!(b.params["approves_unattended"], true);
}

#[test]
fn with_several_approved_budgets_the_most_restrictive_governs() {
    let (q, e) = paid();
    let c = consent("H", true);
    let wide = run_budget("wide", ScopeKind::All, None, 1_000_000, true, 0);
    let narrow = run_budget("narrow", ScopeKind::Focus, Some("focus-1"), 1999, true, 0);
    let b = decide(&c, &q, Some(&e), &[wide, narrow], RunAttendance::Unattended).unwrap_err();
    assert_eq!(b.code, Code::BlockLimitReached);
    assert_eq!(b.params["limit_id"], "narrow");
}

#[test]
fn an_attended_run_needs_no_run_budget() {
    let (q, e) = paid();
    let c = consent("H", true);
    assert!(decide(&c, &q, Some(&e), &[], RunAttendance::Attended).is_ok());
}

#[test]
fn a_call_that_costs_nothing_proceeds_unattended_but_other_limits_still_apply() {
    let q = known(FREE_TEXT);
    let e = est(&q, 1000, 1000);
    let c = consent("H", false);
    assert!(decide(&c, &q, Some(&e), &[], RunAttendance::Unattended).is_ok());
    let full = limit(
        "l",
        LimitMetric::Requests,
        LimitPeriod::Day,
        ScopeKind::All,
        None,
        1,
        false,
        UsageTotals {
            requests: 1,
            ..Default::default()
        },
    );
    assert_eq!(
        code_of(decide(&c, &q, Some(&e), &[full], RunAttendance::Unattended)),
        "spend_block.limit_reached"
    );
}

// ---------------------------------------------------------------------------
// Settlement (pure)
// ---------------------------------------------------------------------------

fn reserved() -> Reserved {
    Reserved {
        reservation_id: "res-1".to_owned(),
        quote: known(PRICE_TEXT),
        input_tokens: 1000,
        output_limit: 1000,
        worst_case_micros: 2000,
        notices: vec![],
        model_id: "fixture-model-a".to_owned(),
        prompt_bytes: 4000,
        token_factor_permille: 1000,
        row_overhead_permille: 1000,
    }
}

fn ok_outcome(content: &str, truncated: bool) -> HostedOutcome {
    HostedOutcome {
        response: GenerateResponse {
            content: content.to_owned(),
            model: "fixture-model-a".to_owned(),
            prompt_token_count: 0,
            output_token_count: 0,
            latency_ms: 1.0,
            completion_status: CompletionStatus::Complete,
        },
        finish_reason: Some("stop".to_owned()),
        reasoning_tokens: None,
        rate_limit: RateLimitInfo::default(),
        truncated,
    }
}

fn provider_err() -> ProviderError {
    ProviderError::Timeout {
        provider: "fixture-host-a".to_owned(),
        plain_language: "x".to_owned(),
    }
}

fn attempt(
    result: Result<HostedOutcome, ProviderError>,
    usage: Option<HostedUsage>,
    http_status: Option<u16>,
    reached: bool,
) -> HostedAttempt {
    HostedAttempt {
        result,
        usage,
        rate_limit: RateLimitInfo::default(),
        http_status,
        reached_provider: reached,
    }
}

fn usage(prompt: u32, completion: u32, reasoning: Option<u32>) -> Option<HostedUsage> {
    Some(HostedUsage {
        prompt_tokens: prompt,
        completion_tokens: completion,
        reasoning_tokens: reasoning,
    })
}

fn settled(a: &HostedAttempt) -> Settlement {
    match settlement_for(&reserved(), a) {
        SettleAction::Settle(s) => s,
        other => panic!("expected a settlement, got {other:?}"),
    }
}

#[test]
fn a_successful_call_is_charged_its_reported_usage_and_records_reasoning() {
    let s = settled(&attempt(
        Ok(ok_outcome("hello", false)),
        usage(2000, 500, Some(300)),
        Some(200),
        true,
    ));
    assert_eq!(s.tokens_in, 2000);
    assert_eq!(s.tokens_out, 500);
    assert_eq!(s.reasoning_tokens, Some(300));
    // 2000 x 500_000/1e6 = 1000, 500 x 1_500_000/1e6 = 750.
    assert_eq!(s.cost_micros, 1750);
    assert_eq!(s.outcome, LedgerOutcome::Ok);
    assert_eq!(s.usage_source, UsageSource::Reported);
}

#[test]
fn a_truncated_answer_is_charged_and_marked() {
    let s = settled(&attempt(
        Ok(ok_outcome("cut off", true)),
        usage(100, 1000, None),
        Some(200),
        true,
    ));
    assert_eq!(s.outcome, LedgerOutcome::Truncated);
    assert_eq!(s.tokens_out, 1000);
    assert!(s.cost_micros > 0);
}

/// The 709 sample: a reasoning model burned the whole output limit and
/// returned an error. The tokens were spent, so they are charged.
#[test]
fn a_budget_exhausted_call_is_charged_for_the_tokens_it_burned() {
    let err = ProviderError::OutputBudgetExhausted {
        provider: "fixture-host-a".to_owned(),
        plain_language: "x".to_owned(),
    };
    let s = settled(&attempt(
        Err(err),
        usage(100, 4096, Some(4096)),
        Some(200),
        true,
    ));
    assert_eq!(s.outcome, LedgerOutcome::BudgetExhausted);
    assert_eq!(s.tokens_out, 4096);
    assert_eq!(s.reasoning_tokens, Some(4096));
    // 100 x 0.5 -> 50, 4096 x 1.5 -> 6144.
    assert_eq!(s.cost_micros, 50 + 6144);
    assert_eq!(s.usage_source, UsageSource::Reported);
}

#[test]
fn a_malformed_answer_that_carried_usage_is_still_charged() {
    let err = ProviderError::MalformedResponse {
        provider: "fixture-host-a".to_owned(),
        plain_language: "x".to_owned(),
    };
    let s = settled(&attempt(Err(err), usage(100, 10, None), Some(200), true));
    assert_eq!(s.outcome, LedgerOutcome::Failed);
    assert_eq!(s.usage_source, UsageSource::Reported);
    assert!(s.cost_micros > 0);
}

#[test]
fn a_success_with_no_usage_is_charged_a_flagged_estimate() {
    // 40 bytes of content -> 10 estimated output tokens; input is the
    // pre-flight estimate.
    let content = "x".repeat(40);
    let s = settled(&attempt(
        Ok(ok_outcome(&content, false)),
        None,
        Some(200),
        true,
    ));
    assert_eq!(s.usage_source, UsageSource::Estimated);
    assert_eq!(s.tokens_in, 1000);
    assert_eq!(s.tokens_out, 10);
    assert_eq!(s.outcome, LedgerOutcome::Ok);
    assert_eq!(s.cost_micros, 500 + 15);
}

#[test]
fn a_provider_error_status_is_a_request_with_no_cost() {
    for code in [429u16, 500, 502, 503, 401, 402, 404] {
        let s = settled(&attempt(Err(provider_err()), None, Some(code), true));
        assert_eq!(s.outcome, LedgerOutcome::Failed, "{code}");
        assert_eq!(s.cost_micros, 0, "{code}");
        assert_eq!(s.tokens_in, 0, "{code}");
        assert_eq!(s.tokens_out, 0, "{code}");
        assert_eq!(s.usage_source, UsageSource::None, "{code}");
    }
}

#[test]
fn a_call_that_reached_the_provider_with_no_usage_is_charged_at_the_worst_case() {
    // A read timeout or reset after sending: no response at all.
    assert_eq!(
        settlement_for(&reserved(), &attempt(Err(provider_err()), None, None, true)),
        SettleAction::Interrupt
    );
    // A 2xx whose body could not be read: also unknown.
    assert_eq!(
        settlement_for(
            &reserved(),
            &attempt(Err(provider_err()), None, Some(200), true)
        ),
        SettleAction::Interrupt
    );
}

#[test]
fn a_call_that_never_left_the_machine_releases_the_reservation() {
    assert_eq!(
        settlement_for(
            &reserved(),
            &attempt(Err(provider_err()), None, None, false)
        ),
        SettleAction::Release
    );
}

// ---------------------------------------------------------------------------
// Source scan: no floating point where amounts live
// ---------------------------------------------------------------------------

/// Production text of a source file: everything before its test module, with
/// line comments removed.
fn production_code(src: &str) -> String {
    let cut = src.find("#[cfg(test)]").unwrap_or(src.len());
    src[..cut]
        .lines()
        .map(|l| match l.find("//") {
            Some(i) => &l[..i],
            None => l,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn assert_no_floats(name: &str, src: &str) {
    let code = production_code(src);
    for needle in ["f64", "f32", "as f64", ".0 *", "powf", "powi"] {
        assert!(
            !code.contains(needle),
            "{name} must hold no floating point (found {needle:?})"
        );
    }
}

#[test]
fn source_scan_finds_no_float_in_ledger_and_cap_paths() {
    assert_no_floats("spend/gate.rs", include_str!("gate.rs"));
    assert_no_floats("spend/estimate.rs", include_str!("estimate.rs"));
    assert_no_floats("spend/calibration.rs", include_str!("calibration.rs"));
    assert_no_floats(
        "persistence/provider_spend_store.rs",
        include_str!("../persistence/provider_spend_store.rs"),
    );
    // money.rs may read a float in exactly one place: the catalog-multiplier
    // entry point `permille_ceil`.
    let code = production_code(include_str!("money.rs"));
    let float_lines: Vec<&str> = code.lines().filter(|l| l.contains("f64")).collect();
    assert_eq!(
        float_lines.len(),
        1,
        "money.rs may mention f64 only in permille_ceil's signature: {float_lines:?}"
    );
    assert!(float_lines[0].contains("fn permille_ceil"));
}

/// No provider or model is named in the spend code or its tests (decisions.id=872).
#[test]
fn the_spend_code_names_no_provider() {
    const SOURCES: [(&str, &str); 8] = [
        ("calibration.rs", include_str!("calibration.rs")),
        ("codes.rs", include_str!("codes.rs")),
        ("disclosure.rs", include_str!("disclosure.rs")),
        ("estimate.rs", include_str!("estimate.rs")),
        ("gate.rs", include_str!("gate.rs")),
        ("money.rs", include_str!("money.rs")),
        ("price.rs", include_str!("price.rs")),
        ("fixtures.rs", include_str!("fixtures.rs")),
    ];
    // Real provider and model names that must not appear.
    let banned = [
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
        "gpt-",
        "haiku",
    ];
    for (file, src) in SOURCES {
        // Skip the seed-data price test, which reads seed SQL by path.
        let lower = src.to_ascii_lowercase();
        for word in banned {
            assert!(
                !lower.contains(word),
                "{file} names a provider or model ({word})"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// The pre-flight against a real encrypted store
// ---------------------------------------------------------------------------

struct Rig {
    provider: Provider,
    models: Vec<ProviderModel>,
    texts: Vec<(String, Option<String>)>,
}

impl Rig {
    fn new(price_text: Option<&str>) -> Self {
        let mut a = model("fixture-model-a");
        a.is_default = true;
        Self {
            provider: provider(),
            models: vec![a],
            texts: vec![("fixture-model-a".to_owned(), price_text.map(str::to_owned))],
        }
    }

    fn call<'a>(&'a self, attendance: RunAttendance, run: Option<&'a str>) -> SpendCall<'a> {
        SpendCall {
            user_id: "spend-user",
            key_hex: TEST_KEY_HEX,
            provider: &self.provider,
            model: &self.models[0],
            all_models: &self.models,
            pricing_texts: &self.texts,
            persona_id: Some("persona-1"),
            focus_id: Some("focus-1"),
            run_id: run,
            attendance,
        }
    }

    fn hash(&self) -> String {
        attributes_hash(&self.provider, &self.models, &self.texts)
    }
}

async fn with_store<F, Fut>(f: F)
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

fn utc_tz() -> FixedOffset {
    FixedOffset::east_opt(0).unwrap()
}

fn t0() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 14, 12, 0, 0).unwrap()
}

async fn consent_now(conn: &mut SqliteConnection, rig: &Rig, no_cap: bool) {
    store::insert_consent_conn(conn, &rig.provider.id, &rig.hash(), no_cap, t0())
        .await
        .unwrap();
}

async fn set_limit(conn: &mut SqliteConnection, spec: LimitSpec) {
    store::upsert_limit_conn(conn, &spec, t0()).await.unwrap();
}

fn spec(
    metric: LimitMetric,
    period: LimitPeriod,
    scope: ScopeKind,
    id: Option<&str>,
    value: i64,
) -> LimitSpec {
    LimitSpec {
        metric,
        period,
        scope_kind: scope,
        scope_id: id.map(str::to_owned),
        limit_value: value,
        warn_at_pct: 80,
        approves_unattended: false,
    }
}

async fn ledger_rows(conn: &mut SqliteConnection) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM provider_usage_ledger")
        .fetch_one(&mut *conn)
        .await
        .unwrap()
}

// 4000 bytes -> 1000 input tokens, 1000 output limit: worst case 2000 micro.
const PROMPT_BYTES: u64 = 4000;
const OUT_LIMIT: u64 = 1000;

#[tokio::test]
async fn a_blocked_preflight_writes_nothing() {
    with_store(|mut conn| async move {
        let rig = Rig::new(Some(PRICE_TEXT));
        // No consent at all.
        let r = preflight(
            &mut conn,
            &rig.call(RunAttendance::Attended, Some("run-1")),
            PROMPT_BYTES,
            OUT_LIMIT,
            &utc_tz(),
            t0(),
        )
        .await;
        assert_eq!(r.unwrap_err().code, Code::BlockConsentRequired);
        assert_eq!(ledger_rows(&mut conn).await, 0);

        // Consent but no cap.
        consent_now(&mut conn, &rig, false).await;
        let r = preflight(
            &mut conn,
            &rig.call(RunAttendance::Attended, Some("run-1")),
            PROMPT_BYTES,
            OUT_LIMIT,
            &utc_tz(),
            t0(),
        )
        .await;
        assert_eq!(r.unwrap_err().code, Code::BlockCapRequired);
        assert_eq!(ledger_rows(&mut conn).await, 0);
    })
    .await;
}

#[tokio::test]
async fn an_unpriced_model_never_reserves() {
    with_store(|mut conn| async move {
        let rig = Rig::new(None);
        consent_now(&mut conn, &rig, true).await;
        let r = preflight(
            &mut conn,
            &rig.call(RunAttendance::Attended, Some("run-1")),
            PROMPT_BYTES,
            OUT_LIMIT,
            &utc_tz(),
            t0(),
        )
        .await;
        assert_eq!(r.unwrap_err().code, Code::BlockPriceUnknown);
        assert_eq!(ledger_rows(&mut conn).await, 0);
    })
    .await;
}

#[tokio::test]
async fn a_user_price_override_makes_an_unpriced_model_callable() {
    with_store(|mut conn| async move {
        let rig = Rig::new(None);
        // The hash does not depend on price amounts, but an unknown cost class
        // differs from a known one; consent is recorded against the unknown.
        consent_now(&mut conn, &rig, true).await;
        sqlx::query("INSERT INTO price_overrides (provider_id, model_id, pricing, updated_at) VALUES ('fixture-host-a', 'fixture-model-a', ?, 't')")
            .bind(r#"{"input_per_mtok":1,"output_per_mtok":2}"#)
            .execute(&mut conn)
            .await
            .unwrap();
        let r = preflight(&mut conn, &rig.call(RunAttendance::Attended, Some("run-1")), PROMPT_BYTES, OUT_LIMIT, &utc_tz(), t0()).await.unwrap();
        // 1000 x 1_000_000/1e6 + 1000 x 2_000_000/1e6 = 3000.
        assert_eq!(r.worst_case_micros, 3000);
        let src: String = sqlx::query_scalar("SELECT price_source FROM provider_usage_ledger WHERE id = ?")
            .bind(&r.reservation_id)
            .fetch_one(&mut conn)
            .await
            .unwrap();
        assert_eq!(src, "user");
    })
    .await;
}

#[tokio::test]
async fn a_cleared_preflight_reserves_the_worst_case() {
    with_store(|mut conn| async move {
        let rig = Rig::new(Some(PRICE_TEXT));
        consent_now(&mut conn, &rig, false).await;
        set_limit(&mut conn, spec(LimitMetric::CostUsd, LimitPeriod::Month, ScopeKind::All, None, 1_000_000)).await;
        let r = preflight(&mut conn, &rig.call(RunAttendance::Attended, Some("run-1")), PROMPT_BYTES, OUT_LIMIT, &utc_tz(), t0()).await.unwrap();
        assert_eq!(r.worst_case_micros, 2000);
        assert_eq!(r.input_tokens, 1000);
        assert_eq!(r.output_limit, 1000);
        let row = sqlx::query("SELECT outcome, tokens_in, tokens_out, requests, cost_micros, reserved_micros, run_id, persona_id, focus_id, price_source FROM provider_usage_ledger WHERE id = ?")
            .bind(&r.reservation_id)
            .fetch_one(&mut conn)
            .await
            .unwrap();
        use sqlx::Row;
        assert_eq!(row.get::<String, _>("outcome"), "pending");
        assert_eq!(row.get::<i64, _>("tokens_in"), 1000);
        assert_eq!(row.get::<i64, _>("tokens_out"), 1000);
        assert_eq!(row.get::<i64, _>("requests"), 1);
        assert_eq!(row.get::<i64, _>("cost_micros"), 2000);
        assert_eq!(row.get::<Option<i64>, _>("reserved_micros"), Some(2000));
        assert_eq!(row.get::<Option<String>, _>("run_id").as_deref(), Some("run-1"));
        assert_eq!(row.get::<Option<String>, _>("persona_id").as_deref(), Some("persona-1"));
        assert_eq!(row.get::<Option<String>, _>("focus_id").as_deref(), Some("focus-1"));
        assert_eq!(row.get::<String, _>("price_source"), "curated");
    })
    .await;
}

/// A reservation counts against the cap before it is settled, so the next
/// call sees it.
#[tokio::test]
async fn a_pending_reservation_counts_against_the_next_call() {
    with_store(|mut conn| async move {
        let rig = Rig::new(Some(PRICE_TEXT));
        consent_now(&mut conn, &rig, false).await;
        // Room for one 2000-micro call, not two.
        set_limit(
            &mut conn,
            spec(
                LimitMetric::CostUsd,
                LimitPeriod::Month,
                ScopeKind::All,
                None,
                3000,
            ),
        )
        .await;
        let call = rig.call(RunAttendance::Attended, Some("run-1"));
        assert!(
            preflight(&mut conn, &call, PROMPT_BYTES, OUT_LIMIT, &utc_tz(), t0())
                .await
                .is_ok()
        );
        let second = preflight(&mut conn, &call, PROMPT_BYTES, OUT_LIMIT, &utc_tz(), t0()).await;
        let b = second.unwrap_err();
        assert_eq!(b.code, Code::BlockLimitReached);
        assert_eq!(b.params["spent"], 2000);
        assert_eq!(
            ledger_rows(&mut conn).await,
            1,
            "the blocked call reserved nothing"
        );
    })
    .await;
}

/// Two calls racing one cap from separate connections: the check and the
/// reservation are one transaction, so exactly one of them clears.
#[tokio::test]
async fn two_concurrent_calls_cannot_both_slip_under_one_cap() {
    with_store(|mut conn| async move {
        let rig = Rig::new(Some(PRICE_TEXT));
        consent_now(&mut conn, &rig, false).await;
        set_limit(
            &mut conn,
            spec(
                LimitMetric::CostUsd,
                LimitPeriod::Month,
                ScopeKind::All,
                None,
                3000,
            ),
        )
        .await;

        let mut a = store::open("spend-user", TEST_KEY_HEX).await.unwrap();
        let mut b = store::open("spend-user", TEST_KEY_HEX).await.unwrap();
        let call_a = rig.call(RunAttendance::Attended, Some("run-a"));
        let call_b = rig.call(RunAttendance::Attended, Some("run-b"));
        let tz = utc_tz();
        let (ra, rb) = tokio::join!(
            preflight(&mut a, &call_a, PROMPT_BYTES, OUT_LIMIT, &tz, t0()),
            preflight(&mut b, &call_b, PROMPT_BYTES, OUT_LIMIT, &tz, t0()),
        );
        let cleared = [ra.is_ok(), rb.is_ok()].iter().filter(|x| **x).count();
        assert_eq!(cleared, 1, "exactly one call may clear: {ra:?} {rb:?}");
        let total = store::usage_totals_conn(&mut conn, &UsageFilter::default())
            .await
            .unwrap();
        assert_eq!(total.cost_micros, 2000);
        assert_eq!(total.requests, 1);
    })
    .await;
}

#[tokio::test]
async fn calendar_periods_ignore_spend_before_the_window() {
    with_store(|mut conn| async move {
        let rig = Rig::new(Some(PRICE_TEXT));
        consent_now(&mut conn, &rig, false).await;
        set_limit(
            &mut conn,
            spec(
                LimitMetric::CostUsd,
                LimitPeriod::Month,
                ScopeKind::All,
                None,
                3000,
            ),
        )
        .await;
        set_limit(
            &mut conn,
            spec(
                LimitMetric::CostUsd,
                LimitPeriod::Day,
                ScopeKind::All,
                None,
                3000,
            ),
        )
        .await;

        // 2000 micro spent LAST month and 2000 spent yesterday.
        let last_month = Utc.with_ymd_and_hms(2026, 9, 30, 23, 0, 0).unwrap();
        let yesterday = Utc.with_ymd_and_hms(2026, 10, 13, 12, 0, 0).unwrap();
        for (run, at) in [("old", last_month), ("yday", yesterday)] {
            store::insert_reservation_conn(
                &mut conn,
                &NewReservation {
                    provider_id: "fixture-host-a",
                    model_id: "fixture-model-a",
                    persona_id: Some("persona-1"),
                    focus_id: Some("focus-1"),
                    run_id: Some(run),
                    tokens_in: 0,
                    tokens_out: 0,
                    cost_micros: 2000,
                    price_source: "curated",
                },
                at,
            )
            .await
            .unwrap();
        }
        // Today (2026-10-14): the month holds only yesterday's 2000, the day
        // holds nothing. Projecting 2000 more: month 4000 > 3000 blocks.
        let call = rig.call(RunAttendance::Attended, Some("run-today"));
        let b = preflight(&mut conn, &call, PROMPT_BYTES, OUT_LIMIT, &utc_tz(), t0())
            .await
            .unwrap_err();
        assert_eq!(b.code, Code::BlockLimitReached);
        assert_eq!(b.params["period"], "month");
        assert_eq!(b.params["spent"], 2000, "last month's spend must not count");

        // Remove the month limit: the day window (nothing today) clears it.
        store::remove_limit_conn(
            &mut conn,
            LimitMetric::CostUsd,
            LimitPeriod::Month,
            ScopeKind::All,
            None,
        )
        .await
        .unwrap();
        assert!(
            preflight(&mut conn, &call, PROMPT_BYTES, OUT_LIMIT, &utc_tz(), t0())
                .await
                .is_ok()
        );
    })
    .await;
}

#[tokio::test]
async fn limits_count_only_their_own_scope() {
    with_store(|mut conn| async move {
        let rig = Rig::new(Some(PRICE_TEXT));
        consent_now(&mut conn, &rig, false).await;
        set_limit(
            &mut conn,
            spec(
                LimitMetric::CostUsd,
                LimitPeriod::Month,
                ScopeKind::All,
                None,
                1_000_000,
            ),
        )
        .await;
        set_limit(
            &mut conn,
            spec(
                LimitMetric::CostUsd,
                LimitPeriod::Day,
                ScopeKind::Persona,
                Some("persona-1"),
                3000,
            ),
        )
        .await;

        // 2000 spent by ANOTHER persona does not count against persona-1.
        store::insert_reservation_conn(
            &mut conn,
            &NewReservation {
                provider_id: "fixture-host-a",
                model_id: "fixture-model-a",
                persona_id: Some("persona-2"),
                focus_id: Some("focus-9"),
                run_id: Some("run-x"),
                tokens_in: 0,
                tokens_out: 0,
                cost_micros: 2000,
                price_source: "curated",
            },
            t0(),
        )
        .await
        .unwrap();
        let call = rig.call(RunAttendance::Attended, Some("run-1"));
        assert!(
            preflight(&mut conn, &call, PROMPT_BYTES, OUT_LIMIT, &utc_tz(), t0())
                .await
                .is_ok()
        );
        // Now persona-1 has 2000 reserved; another 2000 would exceed 3000.
        let b = preflight(&mut conn, &call, PROMPT_BYTES, OUT_LIMIT, &utc_tz(), t0())
            .await
            .unwrap_err();
        assert_eq!(b.params["scope_kind"], "persona");
        assert_eq!(b.params["scope_id"], "persona-1");
    })
    .await;
}

#[tokio::test]
async fn a_per_run_limit_counts_only_that_run() {
    with_store(|mut conn| async move {
        let rig = Rig::new(Some(PRICE_TEXT));
        consent_now(&mut conn, &rig, false).await;
        set_limit(
            &mut conn,
            spec(
                LimitMetric::CostUsd,
                LimitPeriod::Month,
                ScopeKind::All,
                None,
                1_000_000,
            ),
        )
        .await;
        let mut run_cap = spec(
            LimitMetric::CostUsd,
            LimitPeriod::Run,
            ScopeKind::All,
            None,
            3000,
        );
        run_cap.approves_unattended = true;
        set_limit(&mut conn, run_cap).await;

        // The sweep resumes a run with no user present.
        let one = rig.call(RunAttendance::Unattended, Some("run-1"));
        assert!(
            preflight(&mut conn, &one, PROMPT_BYTES, OUT_LIMIT, &utc_tz(), t0())
                .await
                .is_ok()
        );
        let b = preflight(&mut conn, &one, PROMPT_BYTES, OUT_LIMIT, &utc_tz(), t0())
            .await
            .unwrap_err();
        assert_eq!(b.code, Code::BlockLimitReached);
        assert_eq!(b.params["period"], "run");
        // A different run starts from zero against the same approval.
        let other = rig.call(RunAttendance::Unattended, Some("run-2"));
        assert!(
            preflight(&mut conn, &other, PROMPT_BYTES, OUT_LIMIT, &utc_tz(), t0())
                .await
                .is_ok()
        );
    })
    .await;
}

#[tokio::test]
async fn an_unattended_call_parks_without_an_approval_and_writes_nothing() {
    with_store(|mut conn| async move {
        let rig = Rig::new(Some(PRICE_TEXT));
        consent_now(&mut conn, &rig, true).await;
        let call = rig.call(RunAttendance::Unattended, Some("run-1"));
        let b = preflight(&mut conn, &call, PROMPT_BYTES, OUT_LIMIT, &utc_tz(), t0())
            .await
            .unwrap_err();
        assert_eq!(b.code, Code::BlockBudgetApprovalRequired);
        assert_eq!(b.params["estimate_micros"], 2000);
        assert_eq!(ledger_rows(&mut conn).await, 0);
        // The same call, attended, is fine.
        let attended = rig.call(RunAttendance::Attended, Some("run-1"));
        assert!(preflight(
            &mut conn,
            &attended,
            PROMPT_BYTES,
            OUT_LIMIT,
            &utc_tz(),
            t0()
        )
        .await
        .is_ok());
    })
    .await;
}

#[tokio::test]
async fn a_changed_provider_posture_requires_reconsent() {
    with_store(|mut conn| async move {
        let mut rig = Rig::new(Some(PRICE_TEXT));
        consent_now(&mut conn, &rig, true).await;
        let call_ok = preflight(
            &mut conn,
            &rig.call(RunAttendance::Attended, Some("r")),
            PROMPT_BYTES,
            OUT_LIMIT,
            &utc_tz(),
            t0(),
        )
        .await;
        assert!(call_ok.is_ok());
        // The provider now trains on data: a verified attribute changed.
        rig.provider.trains_on_data_by_default = true;
        let b = preflight(
            &mut conn,
            &rig.call(RunAttendance::Attended, Some("r")),
            PROMPT_BYTES,
            OUT_LIMIT,
            &utc_tz(),
            t0(),
        )
        .await
        .unwrap_err();
        assert_eq!(b.code, Code::BlockReconsentRequired);
    })
    .await;
}

#[tokio::test]
async fn settlement_overwrites_the_reservation_with_actual_usage() {
    with_store(|mut conn| async move {
        let rig = Rig::new(Some(PRICE_TEXT));
        consent_now(&mut conn, &rig, true).await;
        let call = rig.call(RunAttendance::Attended, Some("run-1"));
        let r = preflight(&mut conn, &call, PROMPT_BYTES, OUT_LIMIT, &utc_tz(), t0()).await.unwrap();

        let a = HostedAttempt {
            result: Ok(ok_outcome("hi", false)),
            usage: usage(100, 20, Some(5)),
            rate_limit: RateLimitInfo::from_pairs(&[("x-ratelimit-remaining-requests", "9")]),
            http_status: Some(200),
            reached_provider: true,
        };
        finish(&mut conn, &r, "fixture-host-a", &a, t0()).await;

        use sqlx::Row;
        let row = sqlx::query("SELECT outcome, usage_source, tokens_in, tokens_out, reasoning_tokens, cost_micros, reserved_micros FROM provider_usage_ledger WHERE id = ?")
            .bind(&r.reservation_id)
            .fetch_one(&mut conn)
            .await
            .unwrap();
        assert_eq!(row.get::<String, _>("outcome"), "ok");
        assert_eq!(row.get::<String, _>("usage_source"), "reported");
        assert_eq!(row.get::<i64, _>("tokens_in"), 100);
        assert_eq!(row.get::<i64, _>("tokens_out"), 20);
        assert_eq!(row.get::<Option<i64>, _>("reasoning_tokens"), Some(5));
        // 100 x 0.5 -> 50, 20 x 1.5 -> 30.
        assert_eq!(row.get::<i64, _>("cost_micros"), 80);
        assert_eq!(row.get::<Option<i64>, _>("reserved_micros"), Some(2000));
        let snap = store::get_rate_snapshot_conn(&mut conn, "fixture-host-a").await.unwrap().unwrap();
        assert_eq!(snap.requests_remaining, Some(9));
    })
    .await;
}

#[tokio::test]
async fn finish_applies_interrupt_release_and_failed() {
    with_store(|mut conn| async move {
        let rig = Rig::new(Some(PRICE_TEXT));
        consent_now(&mut conn, &rig, true).await;
        let call = rig.call(RunAttendance::Attended, Some("run-1"));

        let timed_out = preflight(&mut conn, &call, PROMPT_BYTES, OUT_LIMIT, &utc_tz(), t0()).await.unwrap();
        finish(&mut conn, &timed_out, "fixture-host-a", &attempt(Err(provider_err()), None, None, true), t0()).await;

        let never_sent = preflight(&mut conn, &call, PROMPT_BYTES, OUT_LIMIT, &utc_tz(), t0()).await.unwrap();
        finish(&mut conn, &never_sent, "fixture-host-a", &attempt(Err(provider_err()), None, None, false), t0()).await;

        let limited = preflight(&mut conn, &call, PROMPT_BYTES, OUT_LIMIT, &utc_tz(), t0()).await.unwrap();
        finish(&mut conn, &limited, "fixture-host-a", &attempt(Err(provider_err()), None, Some(429), true), t0()).await;

        // timed_out stays charged at the worst case; never_sent is gone;
        // limited is one request at no cost.
        let total = store::usage_totals_conn(&mut conn, &UsageFilter::default()).await.unwrap();
        assert_eq!(total.cost_micros, 2000);
        assert_eq!(total.requests, 2);
        assert_eq!(ledger_rows(&mut conn).await, 2);
        let interrupted: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM provider_usage_ledger WHERE outcome = 'interrupted' AND usage_source = 'none'")
            .fetch_one(&mut conn)
            .await
            .unwrap();
        assert_eq!(interrupted, 1);
    })
    .await;
}

// ---------------------------------------------------------------------------
// Calibration (items.id=711)
// ---------------------------------------------------------------------------

async fn calibration_row(
    conn: &mut SqliteConnection,
) -> Option<crate::persistence::provider_spend_store::CalibrationText> {
    store::get_calibration_conn(conn, "fixture-host-a", "fixture-model-a")
        .await
        .unwrap()
}

#[tokio::test]
async fn only_a_reported_ok_call_is_folded_into_calibration() {
    with_store(|mut conn| async move {
        let rig = Rig::new(Some(PRICE_TEXT));
        consent_now(&mut conn, &rig, true).await;
        let call = rig.call(RunAttendance::Attended, Some("run-1"));

        // Truncated, budget exhausted, estimated usage (none reported) and a
        // failed call teach nothing.
        let truncated = preflight(&mut conn, &call, PROMPT_BYTES, OUT_LIMIT, &utc_tz(), t0())
            .await
            .unwrap();
        finish(
            &mut conn,
            &truncated,
            "fixture-host-a",
            &attempt(
                Ok(ok_outcome("hi", true)),
                usage(1300, 20, None),
                Some(200),
                true,
            ),
            t0(),
        )
        .await;
        let estimated = preflight(&mut conn, &call, PROMPT_BYTES, OUT_LIMIT, &utc_tz(), t0())
            .await
            .unwrap();
        finish(
            &mut conn,
            &estimated,
            "fixture-host-a",
            &attempt(Ok(ok_outcome("hi", false)), None, Some(200), true),
            t0(),
        )
        .await;
        let failed = preflight(&mut conn, &call, PROMPT_BYTES, OUT_LIMIT, &utc_tz(), t0())
            .await
            .unwrap();
        finish(
            &mut conn,
            &failed,
            "fixture-host-a",
            &attempt(Err(provider_err()), usage(1300, 20, None), Some(200), true),
            t0(),
        )
        .await;
        assert!(calibration_row(&mut conn).await.is_none());

        // A normal answer with reported usage does. 4000 bytes estimate 1000
        // tokens and the provider counted 1300.
        let ok = preflight(&mut conn, &call, PROMPT_BYTES, OUT_LIMIT, &utc_tz(), t0())
            .await
            .unwrap();
        finish(
            &mut conn,
            &ok,
            "fixture-host-a",
            &attempt(
                Ok(ok_outcome("hi", false)),
                usage(1300, 300, Some(100)),
                Some(200),
                true,
            ),
            t0(),
        )
        .await;
        let c = calibration_row(&mut conn)
            .await
            .expect("a sample was recorded");
        assert_eq!(c.input_factor, "1.3");
        assert_eq!((c.sample_count, c.output_sample_count), (1, 1));
        // 300 completion, 100 reasoning: 1.5x observed against a row of 1.0.
        assert_eq!(c.output_factor, "1.5");
    })
    .await;
}

#[tokio::test]
async fn the_cap_check_never_uses_a_calibration_factor_below_one() {
    with_store(|mut conn| async move {
        let rig = Rig::new(Some(PRICE_TEXT));
        consent_now(&mut conn, &rig, true).await;
        let call = rig.call(RunAttendance::Attended, Some("run-1"));
        let insert = |factor: &'static str| {
            format!("INSERT OR REPLACE INTO calibration (provider_id, model_id, input_factor, output_factor, sample_count, output_sample_count, updated_at) VALUES ('fixture-host-a', 'fixture-model-a', {factor}, 1.0, 9, 0, 't')")
        };

        // Without calibration: 4000 bytes -> 1000 tokens.
        let base = preflight(&mut conn, &call, PROMPT_BYTES, OUT_LIMIT, &utc_tz(), t0()).await.unwrap();
        assert_eq!(base.input_tokens, 1000);

        // A stored factor of 0.5 (nine samples) does NOT lower the estimate.
        sqlx::query(&insert("0.5")).execute(&mut conn).await.unwrap();
        let low = preflight(&mut conn, &call, PROMPT_BYTES, OUT_LIMIT, &utc_tz(), t0()).await.unwrap();
        assert_eq!(low.input_tokens, 1000, "a factor below 1.0 is not applied to the cap check");

        // A stored factor of 2.0 raises it.
        sqlx::query(&insert("2.0")).execute(&mut conn).await.unwrap();
        let high = preflight(&mut conn, &call, PROMPT_BYTES, OUT_LIMIT, &utc_tz(), t0()).await.unwrap();
        assert_eq!(high.input_tokens, 2000);

        // Two samples are not enough to apply even a raising factor.
        sqlx::query("UPDATE calibration SET sample_count = 2").execute(&mut conn).await.unwrap();
        let few = preflight(&mut conn, &call, PROMPT_BYTES, OUT_LIMIT, &utc_tz(), t0()).await.unwrap();
        assert_eq!(few.input_tokens, 1000);
    })
    .await;
}

#[test]
fn block_metadata_carries_code_and_params() {
    let b = SpendBlock::new(Code::BlockLimitReached)
        .with("spent", 5)
        .with("scope_id", Option::<String>::None);
    let m = block_metadata(&b);
    assert_eq!(m["code"], "spend_block.limit_reached");
    assert_eq!(m["spent"], 5);
    assert!(m["scope_id"].is_null());
}

#[test]
fn recovery_window_is_longer_than_the_generate_timeout() {
    // The client's generate timeout is 60 seconds; a reservation younger than
    // that may belong to a live call.
    const { assert!(store::STALE_PENDING_MINUTES * 60 > 60) };
    let _ = Duration::minutes(store::STALE_PENDING_MINUTES);
}
