// src-tauri/src/conductor/run_park_tests.rs
//
// Tests of the pure part of the park reason (items.id=901 stage 7): which failures leave a
// park, what the parameters keep, and how they are fitted to the store's cap. The write, the
// clear and the failure paths against a real outputs.db are in run_park_lifecycle_tests.rs.

use std::collections::HashMap;

use serde_json::{json, Value};

use super::*;
use crate::conductor::failure::{FailureAction, FailureResult, FailureSeverity};
use crate::conductor::step_review::{park_result, BlockReason, Park};
use crate::spend::gate::{block_metadata, SpendBlock};

fn failure_with(meta: Option<HashMap<String, Value>>, action_id: Option<&str>) -> FailureResult {
    FailureResult {
        action: FailureAction::AwaitUser,
        failure_mode: Some("F10".to_owned()),
        plain_language: "x".to_owned(),
        is_recoverable: true,
        severity: FailureSeverity::Require,
        action_id: action_id.map(str::to_owned),
        focus_id: Some("f".to_owned()),
        metadata: meta,
    }
}

/// The failure a spend block becomes (failure.rs maps its params to metadata unchanged).
fn spend_failure(block: &SpendBlock) -> FailureResult {
    let Value::Object(m) = block_metadata(block) else {
        unreachable!()
    };
    failure_with(Some(m.into_iter().collect()), Some("draft"))
}

fn option_entry(i: usize) -> Value {
    json!({
        "subject": "local",
        "model_id": format!("model-{i:03}-{}", "x".repeat(40)),
        "standing": "below_bar",
        "attained": "basic",
    })
}

fn too_weak_failure(n_options: usize, total: Option<usize>) -> FailureResult {
    let options: Vec<Value> = (0..n_options).map(option_entry).collect();
    let mut block = SpendBlock::new(Code::ResolverTooWeak)
        .with("required", "standard")
        .with("best_below_bar", option_entry(0))
        .with("options", Value::Array(options));
    if let Some(t) = total {
        block = block.with("options_total", t);
    }
    spend_failure(&block)
}

fn parsed(p: &ParkRecord) -> serde_json::Map<String, Value> {
    match serde_json::from_str::<Value>(&p.params_json).expect("valid JSON") {
        Value::Object(m) => m,
        other => panic!("not an object: {other}"),
    }
}

// -- which failures leave a park ---------------------------------------------------------

#[test]
fn each_family_leaves_a_park_with_its_registered_code() {
    let cases: Vec<(FailureResult, Code)> = vec![
        (
            spend_failure(&SpendBlock::new(Code::ResolverNoCandidate).with("reason", "none_ready")),
            Code::ResolverNoCandidate,
        ),
        (too_weak_failure(3, None), Code::ResolverTooWeak),
        (
            spend_failure(&SpendBlock::new(Code::BlockLimitReached).with("limit_id", "lim-1")),
            Code::BlockLimitReached,
        ),
        (
            spend_failure(
                &SpendBlock::new(Code::PlanExceeded)
                    .with("action_id", "draft")
                    .with("reason", "no_approval"),
            ),
            Code::PlanExceeded,
        ),
        (
            park_result(&Park::Blocked(BlockReason::Timeout), "draft", "f"),
            Code::StepReviewBlocked,
        ),
        (
            park_result(
                &Park::Pending {
                    review_id: "rv-1".to_owned(),
                },
                "draft",
                "f",
            ),
            Code::StepReviewRequired,
        ),
        (
            park_result(&Park::FilterUnavailable, "draft", "f"),
            Code::StepReviewPrivacyFilterUnavailable,
        ),
        (
            park_result(&Park::TooLong, "draft", "f"),
            Code::StepReviewTooLong,
        ),
    ];
    for (failure, code) in cases {
        let park = from_failure(&failure).unwrap_or_else(|| panic!("{code:?} leaves a park"));
        assert_eq!(park.code, code);
        assert_eq!(park.action_id, "draft");
        assert!(!parsed(&park).contains_key("code"), "the code has a column");
    }
}

#[test]
fn a_step_review_block_carries_its_reason_code() {
    let f = park_result(&Park::Blocked(BlockReason::FilterBlocked), "draft", "f");
    let p = from_failure(&f).unwrap();
    assert_eq!(p.code, Code::StepReviewBlocked);
    assert_eq!(parsed(&p).get("reason"), Some(&json!("filter_blocked")));
}

#[test]
fn a_code_outside_the_registry_or_the_families_leaves_no_park() {
    let by = |code: &str| {
        let mut m = HashMap::new();
        m.insert("code".to_owned(), json!(code));
        from_failure(&failure_with(Some(m), Some("draft")))
    };
    assert!(by("Something went wrong for Jane Doe").is_none());
    assert!(by("resolver.not_a_code").is_none());
    assert!(by("").is_none());
    // Registered, but not a park: a warning, an API Chat stop, a state word.
    for code in [
        Code::WarnCostMetered,
        Code::ApiChatCeilingBlocked,
        Code::PriceStateKnown,
        Code::ConsentStateCurrent,
    ] {
        assert!(by(code.as_str()).is_none(), "{code:?}");
    }
    assert!(from_failure(&failure_with(None, Some("draft"))).is_none());
    // A code that is not a string, and a park with no step to name.
    let mut m = HashMap::new();
    m.insert("code".to_owned(), json!(7));
    assert!(from_failure(&failure_with(Some(m), Some("draft"))).is_none());
    let mut m = HashMap::new();
    m.insert("code".to_owned(), json!("plan.exceeded"));
    assert!(from_failure(&failure_with(Some(m.clone()), None)).is_none());
    assert!(from_failure(&failure_with(Some(m), Some(""))).is_none());
}

#[test]
fn the_families_are_exactly_the_four_and_every_code_has_an_answer() {
    let mut yes = 0;
    for c in Code::ALL {
        let s = c.as_str();
        let in_family = s.starts_with("resolver.")
            || s.starts_with("plan.")
            || s.starts_with("step_review.")
            || s.starts_with("spend_block.");
        assert_eq!(is_park_family(*c), in_family, "{s}");
        yes += usize::from(in_family);
    }
    assert!(yes >= 15, "the four families are all present");
}

// -- what a park carries -----------------------------------------------------------------

#[test]
fn only_ids_counts_and_reason_words_are_kept() {
    let mut m: HashMap<String, Value> = HashMap::new();
    m.insert("code".into(), json!("spend_block.limit_reached"));
    m.insert("limit_id".into(), json!("lim-1"));
    m.insert("spent_micros".into(), json!(12345));
    m.insert("approves_unattended".into(), json!(true));
    m.insert("model_id".into(), json!("llama3.2:3b"));
    m.insert(
        "note".into(),
        json!("Jane Doe's address is 5 Elm St, call her"),
    );
    m.insert("multi_line".into(), json!("a\nb"));
    m.insert("Bad Key".into(), json!("fine"));
    m.insert("long".into(), json!("x".repeat(500)));
    m.insert(
        "nested".into(),
        json!({"ok": "id-1", "text": "has spaces here", "deep": {"a": {"b": {"c": "d"}}}}),
    );
    let got = sanitize_params(&m);
    assert_eq!(got.get("limit_id"), Some(&json!("lim-1")));
    assert_eq!(got.get("spent_micros"), Some(&json!(12345)));
    assert_eq!(got.get("approves_unattended"), Some(&json!(true)));
    assert_eq!(got.get("model_id"), Some(&json!("llama3.2:3b")));
    for dropped in ["code", "note", "multi_line", "Bad Key", "long"] {
        assert!(!got.contains_key(dropped), "{dropped}");
    }
    let nested = got.get("nested").unwrap();
    assert_eq!(nested.get("ok"), Some(&json!("id-1")));
    assert!(nested.get("text").is_none());
    assert!(nested["deep"]["a"].get("b").is_none(), "depth is bounded");
}

#[test]
fn the_same_failure_always_stores_the_same_text() {
    let a = from_failure(&too_weak_failure(4, None)).unwrap();
    let b = from_failure(&too_weak_failure(4, None)).unwrap();
    assert_eq!(a.params_json, b.params_json);
}

// -- resolver options and the trim -------------------------------------------------------

#[test]
fn a_too_weak_park_carries_the_options_in_rank_order() {
    let p = from_failure(&too_weak_failure(5, None)).unwrap();
    let m = parsed(&p);
    assert_eq!(m.get("required"), Some(&json!("standard")));
    let opts = m.get("options").and_then(Value::as_array).unwrap();
    assert_eq!(opts.len(), 5);
    for (i, o) in opts.iter().enumerate() {
        assert_eq!(o, &option_entry(i), "rank order is kept");
    }
    assert_eq!(m.get("options_total"), Some(&json!(5)));
    assert!(!m.contains_key("options_omitted"), "nothing was left out");
    assert_eq!(m["best_below_bar"], option_entry(0));
    assert!(p.params_json.len() <= PARAMS_MAX_BYTES);
}

#[test]
fn a_producer_that_capped_its_options_reports_how_many_are_not_carried() {
    let p = from_failure(&too_weak_failure(16, Some(21))).unwrap();
    let m = parsed(&p);
    assert_eq!(m["options"].as_array().unwrap().len(), 16);
    assert_eq!(m["options_total"], json!(21));
    assert_eq!(m["options_omitted"], json!(5));
}

#[test]
fn options_over_the_cap_are_trimmed_from_the_worst_rank_and_counted() {
    // 16 options of about 130 bytes each plus best_below_bar fit; 60 do not.
    let big = from_failure(&too_weak_failure(60, None)).unwrap();
    assert!(big.params_json.len() <= PARAMS_MAX_BYTES);
    let m = parsed(&big);
    let kept = m["options"].as_array().unwrap();
    assert!(!kept.is_empty() && kept.len() < 60);
    // The best-ranked are kept, in order.
    for (i, o) in kept.iter().enumerate() {
        assert_eq!(o, &option_entry(i));
    }
    assert_eq!(m["options_total"], json!(60));
    assert_eq!(m["options_omitted"], json!(60 - kept.len()));
    // The trim is maximal: one more option would not have fit.
    let mut more = m.clone();
    let mut longer = kept.clone();
    longer.push(option_entry(kept.len()));
    more.insert("options".into(), Value::Array(longer));
    more.insert("options_omitted".into(), json!(60 - kept.len() - 1));
    assert!(serde_json::to_string(&more).unwrap().len() > PARAMS_MAX_BYTES);
    // Deterministic.
    assert_eq!(
        big.params_json,
        from_failure(&too_weak_failure(60, None))
            .unwrap()
            .params_json
    );
}

#[test]
fn a_trim_that_must_drop_every_option_still_stores_complete_json() {
    // best_below_bar and the scalars alone leave room for no option at all.
    let filler: Vec<Value> = (0..30)
        .map(|i| json!({"k": format!("w{i}-{}", "z".repeat(100))}))
        .collect();
    let block = SpendBlock::new(Code::ResolverTooWeak)
        .with("required", "standard")
        .with("padding", Value::Array(filler))
        .with("options", Value::Array((0..3).map(option_entry).collect()));
    let p = from_failure(&spend_failure(&block)).unwrap();
    assert!(p.params_json.len() <= PARAMS_MAX_BYTES);
    let m = parsed(&p);
    assert_eq!(m.get("required"), Some(&json!("standard")));
    // Whatever was cut, what is stored is whole: every key's value is intact JSON, and the
    // options that remain are a prefix.
    if let Some(kept) = m.get("options").and_then(Value::as_array) {
        for (i, o) in kept.iter().enumerate() {
            assert_eq!(o, &option_entry(i));
        }
    }
}

#[test]
fn parameters_too_large_even_without_options_fall_back_to_scalars_then_to_an_empty_object() {
    let mut huge = serde_json::Map::new();
    huge.insert("required".into(), json!("standard"));
    huge.insert(
        "many".into(),
        Value::Array(vec![json!("a".repeat(120)); 32]),
    );
    huge.insert(
        "many2".into(),
        Value::Array(vec![json!("b".repeat(120)); 32]),
    );
    let t = fit(huge);
    let v: Value = serde_json::from_str(&t).expect("valid JSON");
    assert_eq!(v, json!({"required": "standard"}));

    let mut wide = serde_json::Map::new();
    for i in 0..200 {
        wide.insert(
            format!("k{i:03}_{}", "y".repeat(30)),
            json!("v".repeat(100)),
        );
    }
    let t = fit(wide);
    assert_eq!(t, "{}");
}

#[test]
fn fit_never_produces_partial_json_for_any_option_count() {
    for n in [0usize, 1, 2, 15, 16, 17, 40, 100] {
        let p = from_failure(&too_weak_failure(n, None)).unwrap();
        assert!(p.params_json.len() <= PARAMS_MAX_BYTES, "n={n}");
        let m = parsed(&p);
        let kept = m["options"].as_array().unwrap().len();
        let total = m["options_total"].as_u64().unwrap() as usize;
        assert_eq!(total, n);
        assert_eq!(
            m.get("options_omitted")
                .and_then(Value::as_u64)
                .unwrap_or(0) as usize,
            total - kept
        );
    }
}

// -- debug and source ---------------------------------------------------------------------

#[test]
fn debug_shows_the_code_only() {
    let p = from_failure(&too_weak_failure(3, None)).unwrap();
    let shown = format!("{p:?}");
    assert_eq!(shown, "ParkRecord(resolver.too_weak)");
}

#[test]
fn the_module_logs_nothing_and_touches_no_store() {
    let src = include_str!("run_park.rs");
    let code = src.split("#[cfg(test)]").next().unwrap();
    for banned in [
        "log::",
        "println!",
        "eprintln!",
        "dbg!",
        "tracing::",
        "sqlx",
        "run_park_store::put",
        "run_park_store::clear",
    ] {
        assert!(!code.contains(banned), "run_park.rs uses {banned}");
    }
}
