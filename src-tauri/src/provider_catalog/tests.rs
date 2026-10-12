// src-tauri/src/provider_catalog/tests.rs
//
// Neutral fixtures on a real migrated shared.db, loopback only. No provider is
// ever contacted and no real key is involved. The signature tests that need a
// signed fixture are in the separate `signed` module (see its header).

use chrono::{NaiveDate, TimeZone, Utc};
use serde_json::{json, Value};
use sqlx::{Row, SqlitePool};

use super::apply::*;
use super::flow::*;
use super::format::*;
use super::verify::*;
use super::*;
use crate::persistence::provider_catalog_store as store;
use crate::persistence::provider_store::{
    create_provider, NewProvider, PrivacyGuardianDefaultLevel, ProviderMode,
};

const HOST_A: &str = "fixture-host-a";
const HOST_B: &str = "fixture-host-b";
const LOCAL: &str = "fixture-local";

fn today() -> NaiveDate {
    NaiveDate::from_ymd_opt(2026, 10, 20).unwrap()
}

fn now() -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 20, 12, 0, 0).unwrap()
}

const PAID: &str = r#"{"input_per_mtok":0.50,"output_per_mtok":1.50,"currency":"USD","cache":null,"tiers":null,"as_of":"2026-10-01","source_url":"https://prices.fixture-a.test","source":"curated"}"#;
const FREE: &str = r#"{"input_per_mtok":0,"output_per_mtok":0,"currency":"USD","cache":null,"tiers":null,"as_of":"2026-10-01","source_url":null,"source":"curated"}"#;

async fn with_db<F, Fut>(f: F)
where
    F: FnOnce(SqlitePool) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let _lock = crate::test_support::ENV_MUTEX.lock().await;
    let saved = std::env::var("QR_DATA_ROOT").ok();
    let dir = tempfile::tempdir().expect("tempdir");
    std::env::set_var("QR_DATA_ROOT", dir.path());
    crate::persistence::migrations::migrate_shared_db()
        .await
        .expect("migrate");
    let pool = SqlitePool::connect_with(crate::providers::utils::connect_options_unencrypted(
        &crate::providers::utils::db_path_shared(),
    ))
    .await
    .expect("pool");
    f(pool).await;
    match saved {
        Some(v) => std::env::set_var("QR_DATA_ROOT", v),
        None => std::env::remove_var("QR_DATA_ROOT"),
    }
}

async fn make_provider(pool: &SqlitePool, id: &str, local: bool) {
    let gate = json!({"disclosures": ["Fixture disclosure."], "model_overrides": [], "jurisdiction": "Fixtureland"});
    create_provider(
        pool,
        NewProvider {
            id,
            display_name: "Fixture",
            provider_type: "cloud_inference_api",
            mode: if local {
                ProviderMode::Local
            } else {
                ProviderMode::Api
            },
            launch_url: None,
            login_required: false,
            is_local: local,
            is_anonymous: true,
            retains_data: false,
            trains_on_data_by_default: false,
            qr_internal_eligible: false,
            privacy_guardian_default_level: PrivacyGuardianDefaultLevel::Low,
            risk_rating: 1,
            hardware_requirement: None,
            documentation_gate: &gate,
            qr_recommended: false,
            privacy_commitment_basis: None,
            performance_profile: None,
            local_model_tag: None,
            focus_eligible: false,
            cloud_chat_visible: false,
        },
    )
    .await
    .expect("create provider");
    if local {
        return;
    }
    sqlx::query(
        "UPDATE providers SET api_dialect = 'openai_chat',
            base_url_template = 'https://api.fixture.test/v1', auth_scheme = 'bearer',
            extra_headers = '{}', models_path = '/models', user_variables = '[]',
            free_tier = '{\"available\":true,\"as_of\":\"2026-10-01\",\"limits\":{\"rpm\":10}}',
            plan_inclusion = '[]', verification_status = 'verified',
            credential_acquisition = '{\"method\":\"guided_paste\",\"auth_url\":null,\"key_pattern\":null,\"keys_page_url\":\"https://console.fixture.test/keys\"}',
            spend_limit_url = 'https://console.fixture.test/limits',
            required_request_options = '{\"provider\":{\"data_collection\":\"deny\"}}',
            last_reviewed_at = '2026-10-01', review_trigger_note = 'Fixture note.'
          WHERE id = ?",
    )
    .bind(id)
    .execute(pool)
    .await
    .unwrap();
}

#[allow(clippy::too_many_arguments)]
async fn make_model(
    pool: &SqlitePool,
    provider: &str,
    model: &str,
    is_default: bool,
    context: i64,
    pricing: &str,
    options: &str,
    status: &str,
) {
    sqlx::query(
        "INSERT INTO provider_models
            (id, provider_id, model_id, context_window_tokens, is_default, created_at,
             pricing, request_options, status, context_window_source, context_window_as_of)
         VALUES (?, ?, ?, ?, ?, datetime('now'), ?, ?, ?, 'curated', '2026-10-01')",
    )
    .bind(format!("{provider}:{model}"))
    .bind(provider)
    .bind(model)
    .bind(context)
    .bind(i64::from(is_default))
    .bind(pricing)
    .bind(options)
    .bind(status)
    .execute(pool)
    .await
    .unwrap();
}

async fn seed(pool: &SqlitePool) {
    make_provider(pool, HOST_A, false).await;
    make_provider(pool, HOST_B, false).await;
    make_provider(pool, LOCAL, true).await;
    make_model(
        pool,
        HOST_A,
        "model-1",
        true,
        100_000,
        PAID,
        r#"{"max_tokens":1000,"keep":"me"}"#,
        "active",
    )
    .await;
    make_model(
        pool,
        HOST_A,
        "model-2",
        false,
        50_000,
        FREE,
        r#"{"max_completion_tokens":2000}"#,
        "active",
    )
    .await;
    make_model(
        pool,
        HOST_A,
        "model-3",
        false,
        50_000,
        PAID,
        r#"{"max_tokens":500}"#,
        "retired",
    )
    .await;
    make_model(
        pool,
        HOST_B,
        "only",
        true,
        8_000,
        PAID,
        r#"{"max_tokens":800}"#,
        "active",
    )
    .await;
    sqlx::query("UPDATE provider_models SET retains_data_override = 1 WHERE id = ?")
        .bind(format!("{HOST_A}:model-2"))
        .execute(pool)
        .await
        .unwrap();
}

fn doc(v: Value) -> CatalogDoc {
    let mut v = v;
    let o = v.as_object_mut().unwrap();
    o.entry("format").or_insert(json!(1));
    o.entry("catalog_version").or_insert(json!(2026101501u64));
    serde_json::from_value(v).expect("fixture catalog parses")
}

async fn run(pool: &SqlitePool, d: &CatalogDoc) -> Result<Outcome, Reject> {
    let mut tx = pool.begin().await.unwrap();
    match apply_catalog(&mut tx, d, today()).await {
        Ok(o) => {
            tx.commit().await.unwrap();
            Ok(o)
        }
        Err(e) => {
            tx.rollback().await.unwrap();
            Err(e)
        }
    }
}

/// Every column of both tables for every row, as text, for before/after
/// comparisons.
async fn dump(pool: &SqlitePool) -> String {
    let mut out = String::new();
    for (table, id_col) in [("providers", "id"), ("provider_models", "id")] {
        let cols: Vec<String> = sqlx::query(&format!("PRAGMA table_info({table})"))
            .fetch_all(pool)
            .await
            .unwrap()
            .iter()
            .map(|r| r.get::<String, _>("name"))
            .collect();
        let sel: Vec<String> = cols.iter().map(|c| format!("quote({c})")).collect();
        let sql = format!(
            "SELECT {} AS r FROM {table} ORDER BY {id_col}",
            sel.join(" || '|' || ")
        );
        for r in sqlx::query(&sql).fetch_all(pool).await.unwrap() {
            out.push_str(&format!("{table}:{}\n", r.get::<String, _>("r")));
        }
    }
    out
}

async fn col_text(pool: &SqlitePool, table: &str, id: &str, col: &str) -> Option<String> {
    sqlx::query_scalar(&format!("SELECT {col} FROM {table} WHERE id = ?"))
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

fn effect_reasons(o: &Outcome, provider: &str) -> Vec<&'static str> {
    o.consent_effects
        .iter()
        .find(|e| e.provider_id == provider)
        .map(|e| e.reasons.clone())
        .unwrap_or_default()
}

fn price(input: &str, output: &str, as_of: &str) -> Value {
    json!({"input_per_mtok": input, "output_per_mtok": output, "as_of": as_of,
           "source_url": "https://prices.fixture-a.test"})
}

// ---------------------------------------------------------------------------
// Format: the allowlist is structural
// ---------------------------------------------------------------------------

#[test]
fn the_format_cannot_express_a_release_only_field() {
    let bad_provider_fields = [
        "base_url_template",
        "api_dialect",
        "auth_scheme",
        "extra_headers",
        "user_variables",
        "models_path",
        "launch_url",
        "is_local",
        "required_request_options",
        "documentation_gate",
        "jurisdiction",
        "trains_on_data_by_default",
        "retains_data",
        "retention_days",
        "is_anonymous",
        "login_required",
        "risk_rating",
        "verification_status",
        "privacy_commitment_basis",
        "auth_url",
        "credential_acquisition",
        "activation_status",
    ];
    for f in bad_provider_fields {
        let v = json!({"format":1,"catalog_version":2026101501u64,
                       "providers":[{"id":"x", f: "v"}]});
        assert!(
            parse_catalog(v.to_string().as_bytes()).is_err(),
            "provider field {f} must be rejected by the parser"
        );
    }
    let bad_model_fields = [
        "request_options",
        "retains_data_override",
        "trains_on_data_override",
        "retention_days_override",
        "output_limit_key",
    ];
    for f in bad_model_fields {
        let v = json!({"format":1,"catalog_version":2026101501u64,
                       "models":[{"provider_id":"x","model_id":"m", f: 1}]});
        assert!(
            parse_catalog(v.to_string().as_bytes()).is_err(),
            "model field {f} must be rejected by the parser"
        );
    }
    // A new provider has no place in the format at all.
    let v = json!({"format":1,"catalog_version":2026101501u64,"new_providers":[]});
    assert!(parse_catalog(v.to_string().as_bytes()).is_err());
    // The manifest is closed as well.
    let m = json!({"format":1,"catalog_version":2026101501u64,"min_shared_schema":30,
                   "asset_name":"provider-catalog.json","size":1,"sha256":"00","extra":1});
    assert!(parse_manifest(m.to_string().as_bytes()).is_err());
}

#[test]
fn the_writable_column_lists_exclude_every_protected_column() {
    let protected_provider = [
        "required_request_options",
        "documentation_gate",
        "base_url_template",
        "api_dialect",
        "auth_scheme",
        "extra_headers",
        "models_path",
        "user_variables",
        "trains_on_data_by_default",
        "retains_data",
        "retention_days",
        "is_anonymous",
        "login_required",
        "jurisdiction",
        "risk_rating",
        "verification_status",
        "privacy_commitment_basis",
        "is_local",
        "launch_url",
    ];
    let governed = governed_provider_columns();
    for c in protected_provider {
        assert!(!governed.contains(&c), "{c} must not be catalog-writable");
    }
    let protected_model = [
        "retains_data_override",
        "trains_on_data_override",
        "retention_days_override",
    ];
    let governed = governed_model_columns();
    for c in protected_model {
        assert!(!governed.contains(&c), "{c} must not be catalog-writable");
    }
}

#[tokio::test]
async fn every_writable_column_exists() {
    with_db(|pool| async move {
        for (table, cols) in [
            ("providers", governed_provider_columns()),
            ("provider_models", governed_model_columns()),
        ] {
            let existing: Vec<String> = sqlx::query(&format!("PRAGMA table_info({table})"))
                .fetch_all(&pool)
                .await
                .unwrap()
                .iter()
                .map(|r| r.get::<String, _>("name"))
                .collect();
            for c in cols {
                assert!(existing.iter().any(|e| e == c), "{table}.{c} must exist");
            }
        }
    })
    .await;
}

// ---------------------------------------------------------------------------
// Apply: what may change and the consent effect of each
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_price_refresh_in_the_same_cost_class_asks_for_no_consent() {
    with_db(|pool| async move {
        seed(&pool).await;
        let before = col_text(&pool, "providers", HOST_A, "documentation_gate").await;
        let d = doc(json!({"models":[{
            "provider_id": HOST_A, "model_id": "model-1",
            "pricing": price("0.70", "2.10", "2026-10-15"),
            "context_window": {"tokens": 120000, "as_of": "2026-10-15"},
            "output_limit_tokens": 1500,
            "token_factor": 1.2, "output_overhead": 1.5,
            "qr_recommended": true, "qr_recommendation_rank": 2,
            "requires_paid": true
        }]}));
        let out = run(&pool, &d).await.unwrap();
        assert!(out.consent_effects.is_empty(), "{:?}", out.consent_effects);
        let fields: Vec<&str> = out.changes.iter().map(|c| c.field).collect();
        for f in [
            "pricing",
            "context_window",
            "output_limit_tokens",
            "token_factor",
            "output_overhead",
            "qr_recommended",
            "qr_recommendation_rank",
            "requires_paid",
        ] {
            assert!(fields.contains(&f), "missing change {f}: {fields:?}");
        }
        let id = format!("{HOST_A}:model-1");
        let stored = col_text(&pool, "provider_models", &id, "pricing")
            .await
            .unwrap();
        assert!(stored.contains("\"input_per_mtok\":0.70"), "{stored}");
        let opts: Value = serde_json::from_str(
            &col_text(&pool, "provider_models", &id, "request_options")
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(opts, json!({"max_tokens":1500,"keep":"me"}));
        assert_eq!(
            col_text(&pool, "providers", HOST_A, "documentation_gate").await,
            before
        );
    })
    .await;
}

#[tokio::test]
async fn a_price_that_flips_the_cost_class_asks_for_consent_again() {
    with_db(|pool| async move {
        seed(&pool).await;
        // free -> metered
        let d = doc(json!({"models":[{
            "provider_id": HOST_A, "model_id": "model-2",
            "pricing": price("0.10", "0.20", "2026-10-15")}]}));
        let out = run(&pool, &d).await.unwrap();
        assert_eq!(
            effect_reasons(&out, HOST_A),
            vec![codes::CONSENT_COST_CLASS_CHANGED]
        );
        // metered -> free
        let d = doc(json!({"catalog_version": 2026101502u64, "models":[{
            "provider_id": HOST_B, "model_id": "only",
            "pricing": price("0", "0", "2026-10-15")}]}));
        let out = run(&pool, &d).await.unwrap();
        assert_eq!(
            effect_reasons(&out, HOST_B),
            vec![codes::CONSENT_COST_CLASS_CHANGED]
        );
    })
    .await;
}

#[tokio::test]
async fn adding_and_retiring_models_ask_for_consent_again() {
    with_db(|pool| async move {
        seed(&pool).await;
        let d = doc(json!({"new_models":[{
            "provider_id": HOST_A, "model_id": "model-4",
            "pricing": price("1.00", "2.00", "2026-10-15"),
            "context_window": {"tokens": 64000, "as_of": "2026-10-15"},
            "output_limit": {"key": "max_tokens", "tokens": 700}
        }]}));
        let out = run(&pool, &d).await.unwrap();
        assert_eq!(
            effect_reasons(&out, HOST_A),
            vec![codes::CONSENT_MODEL_ADDED]
        );
        let id = format!("{HOST_A}:model-4");
        // No privacy override, no request option beyond the limit.
        for col in [
            "retains_data_override",
            "trains_on_data_override",
            "retention_days_override",
        ] {
            assert_eq!(col_text(&pool, "provider_models", &id, col).await, None);
        }
        assert_eq!(
            col_text(&pool, "provider_models", &id, "request_options").await,
            Some(r#"{"max_tokens":700}"#.to_owned())
        );
        // Retire it again (not the default, so allowed).
        let d = doc(json!({"catalog_version": 2026101502u64, "models":[{
            "provider_id": HOST_A, "model_id": "model-4", "status": "retired"}]}));
        let out = run(&pool, &d).await.unwrap();
        assert_eq!(
            effect_reasons(&out, HOST_A),
            vec![codes::CONSENT_MODEL_RETIRED]
        );
        // A deprecation alone does not change the live set.
        let d = doc(json!({"catalog_version": 2026101503u64, "models":[{
            "provider_id": HOST_A, "model_id": "model-2", "status": "deprecated",
            "retires_at": "2026-12-31"}]}));
        let out = run(&pool, &d).await.unwrap();
        assert!(out.consent_effects.is_empty());
    })
    .await;
}

#[tokio::test]
async fn non_governed_columns_are_untouched_by_a_full_featured_apply() {
    with_db(|pool| async move {
        seed(&pool).await;
        let protected = |pool: SqlitePool| async move {
            let mut out = Vec::new();
            for id in [HOST_A, HOST_B, LOCAL] {
                for c in [
                    "required_request_options",
                    "documentation_gate",
                    "base_url_template",
                    "api_dialect",
                    "trains_on_data_by_default",
                    "retains_data",
                    "risk_rating",
                    "jurisdiction",
                    "verification_status",
                    "auth_scheme",
                    "models_path",
                ] {
                    out.push(col_text(&pool, "providers", id, &format!("CAST({c} AS TEXT)")).await);
                }
            }
            for id in [
                format!("{HOST_A}:model-1"),
                format!("{HOST_A}:model-2"),
                format!("{HOST_B}:only"),
            ] {
                for c in [
                    "retains_data_override",
                    "trains_on_data_override",
                    "retention_days_override",
                ] {
                    out.push(
                        col_text(&pool, "provider_models", &id, &format!("CAST({c} AS TEXT)"))
                            .await,
                    );
                }
                let opts = col_text(&pool, "provider_models", &id, "request_options").await;
                let mut v: Value = serde_json::from_str(&opts.unwrap()).unwrap();
                let m = v.as_object_mut().unwrap();
                m.remove("max_tokens");
                m.remove("max_completion_tokens");
                out.push(Some(v.to_string()));
            }
            out
        };
        let before = protected(pool.clone()).await;
        let d = doc(json!({
            "providers":[{"id": HOST_A,
                "free_tier": {"available": false, "as_of": "2026-10-15"},
                "plan_inclusion": ["Fixture Plan"], "last_reviewed_at": "2026-10-15",
                "review_trigger_note": "Changed.", "qr_recommended": true,
                "qr_recommendation_rank": 3,
                "spend_limit_url": "https://console.fixture.test/limits/new",
                "keys_page_url": "https://console.fixture.test/keys/new"}],
            "models":[
                {"provider_id": HOST_A, "model_id": "model-1", "output_limit_tokens": 900,
                 "pricing": price("0.9", "1.9", "2026-10-15")},
                {"provider_id": HOST_A, "model_id": "model-2", "output_limit_tokens": 100,
                 "status": "deprecated"}],
            "new_models":[{"provider_id": HOST_B, "model_id": "extra",
                "pricing": price("1", "2", "2026-10-15"),
                "context_window": {"tokens": 4000, "as_of": "2026-10-15"},
                "output_limit": {"key": "max_completion_tokens", "tokens": 300}}]
        }));
        run(&pool, &d).await.unwrap();
        assert_eq!(protected(pool.clone()).await, before);
    })
    .await;
}

#[tokio::test]
async fn a_local_or_unknown_provider_is_refused() {
    with_db(|pool| async move {
        seed(&pool).await;
        for (id, code) in [
            (LOCAL, codes::REJECT_NOT_HOSTED_PROVIDER),
            ("no-such-provider", codes::REJECT_UNKNOWN_PROVIDER),
        ] {
            let d = doc(json!({"providers":[{"id": id, "qr_recommended": true}]}));
            assert_eq!(run(&pool, &d).await.unwrap_err().code, code);
            let d = doc(json!({"models":[{"provider_id": id, "model_id": "m"}]}));
            assert_eq!(run(&pool, &d).await.unwrap_err().code, code);
        }
        // The seeded local models are provider rows too.
        let local_seed: Option<String> =
            sqlx::query_scalar("SELECT id FROM providers WHERE is_local = 1 LIMIT 1")
                .fetch_optional(&pool)
                .await
                .unwrap();
        if let Some(id) = local_seed {
            let d = doc(json!({"providers":[{"id": id, "qr_recommended": true}]}));
            assert_eq!(
                run(&pool, &d).await.unwrap_err().code,
                codes::REJECT_NOT_HOSTED_PROVIDER
            );
        }
    })
    .await;
}

#[tokio::test]
async fn urls_must_stay_on_the_same_host_and_https() {
    with_db(|pool| async move {
        seed(&pool).await;
        let cases = [
            (
                "spend_limit_url",
                "https://evil.test/limits",
                codes::REJECT_URL_HOST_CHANGED,
            ),
            (
                "spend_limit_url",
                "http://console.fixture.test/limits",
                codes::REJECT_BAD_URL,
            ),
            (
                "keys_page_url",
                "https://evil.test/keys",
                codes::REJECT_URL_HOST_CHANGED,
            ),
            (
                "keys_page_url",
                "javascript:alert(1)",
                codes::REJECT_BAD_URL,
            ),
            (
                "keys_page_url",
                "https://user:pw@console.fixture.test/keys",
                codes::REJECT_BAD_URL,
            ),
        ];
        for (field, url, code) in cases {
            let d = doc(json!({"providers":[{"id": HOST_A, field: url}]}));
            assert_eq!(
                run(&pool, &d).await.unwrap_err().code,
                code,
                "{field} {url}"
            );
        }
        // A provider with no stored URL cannot be given one.
        sqlx::query("UPDATE providers SET spend_limit_url = NULL WHERE id = ?")
            .bind(HOST_B)
            .execute(&pool)
            .await
            .unwrap();
        let d = doc(json!({"providers":[{"id": HOST_B,
            "spend_limit_url": "https://console.fixture.test/limits"}]}));
        assert_eq!(
            run(&pool, &d).await.unwrap_err().code,
            codes::REJECT_URL_HOST_CHANGED
        );
    })
    .await;
}

#[tokio::test]
async fn bad_values_are_refused() {
    with_db(|pool| async move {
        seed(&pool).await;
        let m = |extra: Value| {
            let mut base = json!({"provider_id": HOST_A, "model_id": "model-1"});
            base.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            doc(json!({"models":[base]}))
        };
        let cases: Vec<(CatalogDoc, &str)> = vec![
            (
                m(json!({"pricing": price("-1", "2", "2026-10-15")})),
                codes::REJECT_BAD_PRICE,
            ),
            (
                m(json!({"pricing": price("1e3", "2", "2026-10-15")})),
                codes::REJECT_BAD_PRICE,
            ),
            (
                m(json!({"pricing": price("1.1234567", "2", "2026-10-15")})),
                codes::REJECT_BAD_PRICE,
            ),
            (
                m(json!({"pricing": price("1001", "2", "2026-10-15")})),
                codes::REJECT_BAD_PRICE,
            ),
            (
                m(json!({"pricing": price("1", "2", "2026-13-40")})),
                codes::REJECT_BAD_PRICE,
            ),
            (
                m(json!({"pricing": price("1", "2", "2027-01-01")})),
                codes::REJECT_BAD_PRICE,
            ),
            (
                m(
                    json!({"pricing": {"input_per_mtok":"1","output_per_mtok":"2",
                    "as_of":"2026-10-15","source_url":"http://x.test"}}),
                ),
                codes::REJECT_BAD_PRICE,
            ),
            (
                m(
                    json!({"pricing": {"input_per_mtok":"1","output_per_mtok":"2",
                    "as_of":"2026-10-15","tiers":[{"input_per_mtok":"1","output_per_mtok":"2"}]}}),
                ),
                codes::REJECT_BAD_PRICE,
            ),
            (m(json!({"token_factor": 0.0})), codes::REJECT_BAD_VALUE),
            (m(json!({"token_factor": 9.0})), codes::REJECT_BAD_VALUE),
            (m(json!({"output_overhead": 0.5})), codes::REJECT_BAD_VALUE),
            (
                m(json!({"output_overhead": 100.0})),
                codes::REJECT_BAD_VALUE,
            ),
            (
                m(json!({"context_window": {"tokens": 0, "as_of": "2026-10-15"}})),
                codes::REJECT_BAD_VALUE,
            ),
            (m(json!({"retires_at": "soon"})), codes::REJECT_BAD_VALUE),
            (
                m(json!({"output_limit_tokens": 0})),
                codes::REJECT_OUTPUT_LIMIT,
            ),
            // Larger than the context window: caught by the invariant pass.
            (
                m(json!({"output_limit_tokens": 500000})),
                codes::REJECT_OUTPUT_LIMIT,
            ),
            // The default cannot be retired.
            (m(json!({"status": "retired"})), codes::REJECT_DEFAULT_MODEL),
            (
                doc(json!({"providers":[{"id": HOST_A, "last_reviewed_at": "yesterday"}]})),
                codes::REJECT_BAD_VALUE,
            ),
            (
                doc(json!({"providers":[{"id": HOST_A,
                    "free_tier": {"available": "yes", "as_of": "2026-10-15"}}]})),
                codes::REJECT_BAD_VALUE,
            ),
            (
                doc(json!({"providers":[{"id": HOST_A,
                    "free_tier": {"available": true, "as_of": "2026-10-15",
                                  "nested": {"a": {"b": {"c": 1}}}}}]})),
                codes::REJECT_BAD_VALUE,
            ),
            (
                doc(json!({"providers":[{"id": HOST_A, "plan_inclusion": ["a\u{0}b"]}]})),
                codes::REJECT_BAD_VALUE,
            ),
            (
                doc(json!({"models":[{"provider_id": HOST_A, "model_id": "missing"}]})),
                codes::REJECT_UNKNOWN_MODEL,
            ),
            (
                doc(json!({"providers":[{"id": HOST_A}, {"id": HOST_A}]})),
                codes::REJECT_DUPLICATE_ENTRY,
            ),
            (
                doc(
                    json!({"models":[{"provider_id": HOST_A, "model_id": "model-1"},
                                     {"provider_id": HOST_A, "model_id": "model-1"}]}),
                ),
                codes::REJECT_DUPLICATE_ENTRY,
            ),
        ];
        let snapshot = dump(&pool).await;
        for (d, code) in cases {
            assert_eq!(run(&pool, &d).await.unwrap_err().code, code, "{d:?}");
            assert_eq!(
                dump(&pool).await,
                snapshot,
                "a refused catalog writes nothing"
            );
        }
    })
    .await;
}

#[tokio::test]
async fn new_models_are_validated() {
    with_db(|pool| async move {
        seed(&pool).await;
        let nm = |extra: Value| {
            let mut base = json!({
                "provider_id": HOST_A, "model_id": "fresh",
                "pricing": price("1", "2", "2026-10-15"),
                "context_window": {"tokens": 8000, "as_of": "2026-10-15"},
                "output_limit": {"key": "max_tokens", "tokens": 500}});
            base.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            doc(json!({"new_models":[base]}))
        };
        let cases = vec![
            (
                nm(json!({"model_id": "model-1"})),
                codes::REJECT_MODEL_EXISTS,
            ),
            (
                nm(json!({"model_id": "with space"})),
                codes::REJECT_BAD_VALUE,
            ),
            (nm(json!({"model_id": ""})), codes::REJECT_BAD_VALUE),
            (nm(json!({"status": "retired"})), codes::REJECT_BAD_VALUE),
            (
                nm(json!({"output_limit": {"key": "stop", "tokens": 5}})),
                codes::REJECT_OUTPUT_LIMIT,
            ),
            (
                nm(json!({"output_limit": {"key": "max_tokens", "tokens": 9000}})),
                codes::REJECT_OUTPUT_LIMIT,
            ),
            (nm(json!({"token_factor": 100.0})), codes::REJECT_BAD_VALUE),
        ];
        for (d, code) in cases {
            assert_eq!(run(&pool, &d).await.unwrap_err().code, code, "{d:?}");
        }
        // A new default model takes over the slot.
        let out = run(&pool, &nm(json!({"is_default": true}))).await.unwrap();
        assert!(!out.is_noop());
        let defaults: Vec<String> = sqlx::query_scalar(
            "SELECT model_id FROM provider_models WHERE provider_id = ? AND is_default = 1",
        )
        .bind(HOST_A)
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(defaults, vec!["fresh".to_owned()]);
    })
    .await;
}

#[tokio::test]
async fn an_older_price_or_context_does_not_replace_a_newer_one() {
    with_db(|pool| async move {
        seed(&pool).await;
        let d = doc(json!({"models":[{
            "provider_id": HOST_A, "model_id": "model-1",
            "pricing": price("9", "9", "2026-09-01"),
            "context_window": {"tokens": 5, "as_of": "2026-09-01"}}]}));
        let before = dump(&pool).await;
        let out = run(&pool, &d).await.unwrap();
        assert!(out.is_noop());
        let codes_seen: Vec<&str> = out.skipped.iter().map(|s| s.code).collect();
        assert_eq!(
            codes_seen,
            vec![codes::SKIP_PRICE_OLDER, codes::SKIP_CONTEXT_OLDER]
        );
        assert_eq!(dump(&pool).await, before);
    })
    .await;
}

#[tokio::test]
async fn an_unchanged_catalog_is_a_no_op() {
    with_db(|pool| async move {
        seed(&pool).await;
        let d = doc(json!({"models":[{
            "provider_id": HOST_A, "model_id": "model-1",
            "pricing": price("0.50", "1.50", "2026-10-01"),
            "output_limit_tokens": 1000, "token_factor": 1.0}]}));
        let out = run(&pool, &d).await.unwrap();
        assert!(out.is_noop(), "{:?}", out.changes);
        assert!(out.consent_effects.is_empty());
    })
    .await;
}

#[tokio::test]
async fn moving_the_default_keeps_exactly_one() {
    with_db(|pool| async move {
        seed(&pool).await;
        let d = doc(json!({"models":[{
            "provider_id": HOST_A, "model_id": "model-2", "is_default": true}]}));
        run(&pool, &d).await.unwrap();
        let defaults: Vec<String> = sqlx::query_scalar(
            "SELECT model_id FROM provider_models WHERE provider_id = ? AND is_default = 1",
        )
        .bind(HOST_A)
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(defaults, vec!["model-2".to_owned()]);
        // Clearing the only default without naming another is refused.
        let d = doc(json!({"models":[{
            "provider_id": HOST_B, "model_id": "only", "is_default": false}]}));
        assert_eq!(
            run(&pool, &d).await.unwrap_err().code,
            codes::REJECT_DEFAULT_MODEL
        );
    })
    .await;
}

// ---------------------------------------------------------------------------
// The invariant checks catch what the format cannot express
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_fingerprint_sees_release_only_changes_and_ignores_governed_ones() {
    with_db(|pool| async move {
        seed(&pool).await;
        let mut conn = pool.acquire().await.unwrap();
        let base = release_fingerprint(&mut conn).await.unwrap();
        let drift = |a: &std::collections::BTreeMap<String, String>,
                     b: &std::collections::BTreeMap<String, String>| fingerprint_drift(a, b);

        let q = |sql: &'static str| {
            let pool = pool.clone();
            async move {
                sqlx::query(sql).execute(&pool).await.unwrap();
            }
        };
        // Governed columns: no drift.
        q("UPDATE providers SET qr_recommended = 1, free_tier = '{}' WHERE id = 'fixture-host-a'").await;
        q("UPDATE provider_models SET pricing = NULL, is_default = is_default WHERE id = 'fixture-host-a:model-2'").await;
        q("UPDATE provider_models SET request_options = '{\"max_tokens\":1,\"keep\":\"me\"}' WHERE id = 'fixture-host-a:model-1'").await;
        q("UPDATE providers SET credential_acquisition = json_set(credential_acquisition, '$.keys_page_url', 'https://console.fixture.test/other') WHERE id = 'fixture-host-a'").await;
        let now = release_fingerprint(&mut conn).await.unwrap();
        assert_eq!(drift(&base, &now), None);

        // Protected columns: drift.
        for sql in [
            "UPDATE providers SET required_request_options = NULL WHERE id = 'fixture-host-a'",
            "UPDATE providers SET base_url_template = 'https://evil.test' WHERE id = 'fixture-host-a'",
            "UPDATE providers SET trains_on_data_by_default = 1 WHERE id = 'fixture-host-b'",
            "UPDATE providers SET documentation_gate = '{}' WHERE id = 'fixture-host-b'",
            "UPDATE providers SET credential_acquisition = json_set(credential_acquisition, '$.auth_url', 'https://evil.test') WHERE id = 'fixture-host-a'",
            "UPDATE provider_models SET request_options = '{\"max_tokens\":1000}' WHERE id = 'fixture-host-a:model-1'",
            "UPDATE provider_models SET trains_on_data_override = 1 WHERE id = 'fixture-host-b:only'",
            "UPDATE provider_models SET retains_data_override = NULL WHERE id = 'fixture-host-a:model-2'",
        ] {
            let before = release_fingerprint(&mut conn).await.unwrap();
            q(sql).await;
            let after = release_fingerprint(&mut conn).await.unwrap();
            assert!(drift(&before, &after).is_some(), "{sql} must be detected");
        }
    })
    .await;
}

#[tokio::test]
async fn an_unexplained_consent_change_is_refused() {
    with_db(|pool| async move {
        seed(&pool).await;
        let mut conn = pool.acquire().await.unwrap();
        let before = consent_snapshot(&mut conn, HOST_A).await.unwrap();
        sqlx::query("UPDATE providers SET trains_on_data_by_default = 1 WHERE id = ?")
            .bind(HOST_A)
            .execute(&pool)
            .await
            .unwrap();
        let after = consent_snapshot(&mut conn, HOST_A).await.unwrap();
        assert_ne!(before.hash, after.hash, "the hash moved");
        let err = consent_effect(HOST_A, &before, &after).unwrap_err();
        assert_eq!(err.code, codes::REJECT_UNEXPLAINED_CONSENT_CHANGE);
    })
    .await;
}

// ---------------------------------------------------------------------------
// Rollback
// ---------------------------------------------------------------------------

async fn apply_recorded(pool: &SqlitePool, d: &CatalogDoc) -> Outcome {
    let body = serde_json::to_string(&json!({
        "format": d.format, "catalog_version": d.catalog_version}))
    .unwrap();
    let staged = Staged {
        manifest: Manifest {
            format: 1,
            catalog_version: d.catalog_version,
            min_shared_schema: 1,
            asset_name: CATALOG_ASSET_NAME.to_owned(),
            size: 1,
            sha256: "0".repeat(64),
        },
        doc: d.clone(),
        sha256: "0".repeat(64),
        body,
    };
    apply_staged(pool, &staged, today(), now()).await.unwrap()
}

#[tokio::test]
async fn rollback_restores_every_row_and_remembers_the_version() {
    with_db(|pool| async move {
        seed(&pool).await;
        let original = dump(&pool).await;
        let d = doc(json!({
            "catalog_version": 2026101501u64,
            "providers":[{"id": HOST_A, "qr_recommended": true, "last_reviewed_at": "2026-10-15"}],
            "models":[
                {"provider_id": HOST_A, "model_id": "model-2", "is_default": true,
                 "pricing": price("0.4", "0.8", "2026-10-15")},
                {"provider_id": HOST_B, "model_id": "only", "output_limit_tokens": 700}],
            "new_models":[{"provider_id": HOST_B, "model_id": "extra",
                "pricing": price("1", "2", "2026-10-15"),
                "context_window": {"tokens": 4000, "as_of": "2026-10-15"},
                "output_limit": {"key": "max_tokens", "tokens": 300}}]
        }));
        let applied = apply_recorded(&pool, &d).await;
        assert!(!applied.is_noop());
        assert_ne!(dump(&pool).await, original);
        let mut conn = pool.acquire().await.unwrap();
        let st = store::get_state(&mut conn).await.unwrap();
        assert_eq!(st.applied_version, 2026101501);
        drop(conn);

        let out = rollback_latest(&pool, now()).await.unwrap();
        // Rolling back the added model and the free->metered flip re-asks
        // consent exactly where applying did.
        let effects: Vec<&str> = out
            .consent_effects
            .iter()
            .map(|e| e.provider_id.as_str())
            .collect();
        assert!(!effects.is_empty());
        assert_eq!(dump(&pool).await, original, "every row is back as it was");

        let mut conn = pool.acquire().await.unwrap();
        let st = store::get_state(&mut conn).await.unwrap();
        assert_eq!(st.applied_version, 0);
        assert_eq!(st.declined_version, 2026101501);
        drop(conn);
        // Nothing left to roll back.
        assert_eq!(
            rollback_latest(&pool, now()).await.unwrap_err().code,
            codes::ERROR_NOTHING_TO_ROLL_BACK
        );
    })
    .await;
}

#[tokio::test]
async fn rollback_goes_back_one_catalog_at_a_time() {
    with_db(|pool| async move {
        seed(&pool).await;
        let original = dump(&pool).await;
        let first = doc(json!({"catalog_version": 2026101501u64, "models":[{
            "provider_id": HOST_B, "model_id": "only",
            "pricing": price("0.6", "1.6", "2026-10-15")}]}));
        apply_recorded(&pool, &first).await;
        let after_first = dump(&pool).await;
        let second = doc(json!({"catalog_version": 2026101502u64, "models":[{
            "provider_id": HOST_B, "model_id": "only",
            "pricing": price("0.7", "1.7", "2026-10-16")}]}));
        apply_recorded(&pool, &second).await;

        rollback_latest(&pool, now()).await.unwrap();
        assert_eq!(dump(&pool).await, after_first);
        let mut conn = pool.acquire().await.unwrap();
        let st = store::get_state(&mut conn).await.unwrap();
        assert_eq!(st.applied_version, 2026101501);
        assert_eq!(st.declined_version, 2026101502);
        drop(conn);

        rollback_latest(&pool, now()).await.unwrap();
        assert_eq!(dump(&pool).await, original);
    })
    .await;
}

#[tokio::test]
async fn only_the_last_two_catalogs_are_kept() {
    with_db(|pool| async move {
        seed(&pool).await;
        for (i, v) in [2026101501u64, 2026101502, 2026101503]
            .into_iter()
            .enumerate()
        {
            let d = doc(json!({"catalog_version": v, "models":[{
                "provider_id": HOST_B, "model_id": "only",
                "pricing": price(&format!("0.{}", i + 3), "1", "2026-10-15")}]}));
            apply_recorded(&pool, &d).await;
        }
        let kept: Vec<i64> =
            sqlx::query_scalar("SELECT version FROM provider_catalog_history ORDER BY version")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(kept, vec![2026101502, 2026101503]);
    })
    .await;
}

#[tokio::test]
async fn a_failed_apply_records_nothing_and_changes_nothing() {
    with_db(|pool| async move {
        seed(&pool).await;
        let before = dump(&pool).await;
        // The first entry is fine, the second is refused: all or nothing.
        let d = doc(json!({
            "catalog_version": 2026101501u64,
            "models":[
                {"provider_id": HOST_B, "model_id": "only",
                 "pricing": price("0.6", "1.6", "2026-10-15")},
                {"provider_id": HOST_B, "model_id": "missing"}]}));
        let staged = Staged {
            manifest: Manifest {
                format: 1,
                catalog_version: 2026101501,
                min_shared_schema: 1,
                asset_name: CATALOG_ASSET_NAME.to_owned(),
                size: 1,
                sha256: "0".repeat(64),
            },
            doc: d,
            sha256: "0".repeat(64),
            body: "{}".to_owned(),
        };
        let err = apply_staged(&pool, &staged, today(), now())
            .await
            .unwrap_err();
        assert_eq!(err.code, codes::REJECT_UNKNOWN_MODEL);
        assert_eq!(dump(&pool).await, before);
        let mut conn = pool.acquire().await.unwrap();
        assert_eq!(
            store::get_state(&mut conn).await.unwrap().applied_version,
            0
        );
        assert!(store::latest_history(&mut conn).await.unwrap().is_none());
    })
    .await;
}

#[tokio::test]
async fn a_preview_writes_nothing() {
    with_db(|pool| async move {
        seed(&pool).await;
        let before = dump(&pool).await;
        let d = doc(json!({"models":[{
            "provider_id": HOST_A, "model_id": "model-2",
            "pricing": price("0.10", "0.20", "2026-10-15")}]}));
        let out = preview(&pool, &d, today()).await.unwrap();
        assert_eq!(
            effect_reasons(&out, HOST_A),
            vec![codes::CONSENT_COST_CLASS_CHANGED]
        );
        assert_eq!(dump(&pool).await, before);
        let mut conn = pool.acquire().await.unwrap();
        assert_eq!(
            store::get_state(&mut conn).await.unwrap().applied_version,
            0
        );
    })
    .await;
}

#[tokio::test]
async fn rolling_back_before_re_consenting_restores_the_old_consent_hash() {
    with_db(|pool| async move {
        seed(&pool).await;
        let mut conn = pool.acquire().await.unwrap();
        let original = consent_snapshot(&mut conn, HOST_A).await.unwrap();
        drop(conn);
        let d = doc(json!({"models":[{
            "provider_id": HOST_A, "model_id": "model-2",
            "pricing": price("0.10", "0.20", "2026-10-15")}]}));
        apply_recorded(&pool, &d).await;
        let mut conn = pool.acquire().await.unwrap();
        let changed = consent_snapshot(&mut conn, HOST_A).await.unwrap();
        assert_ne!(original.hash, changed.hash);
        drop(conn);
        rollback_latest(&pool, now()).await.unwrap();
        let mut conn = pool.acquire().await.unwrap();
        let back = consent_snapshot(&mut conn, HOST_A).await.unwrap();
        assert_eq!(
            back.hash, original.hash,
            "the stored consent compares equal again"
        );
    })
    .await;
}

// ---------------------------------------------------------------------------
// Versions and the manifest
// ---------------------------------------------------------------------------

fn manifest(version: u64) -> Manifest {
    Manifest {
        format: 1,
        catalog_version: version,
        min_shared_schema: 30,
        asset_name: CATALOG_ASSET_NAME.to_owned(),
        size: 100,
        sha256: "ab".repeat(32),
    }
}

fn state(applied: u64, declined: u64) -> store::CatalogState {
    store::CatalogState {
        applied_version: applied,
        applied_sha256: None,
        applied_at: None,
        declined_version: declined,
    }
}

#[test]
fn version_dates_are_read_from_the_version() {
    assert_eq!(
        version_date(2026101501),
        NaiveDate::from_ymd_opt(2026, 10, 15)
    );
    assert_eq!(version_date(2026133301), None);
    assert_eq!(version_date(2025010101), None, "before the project existed");
    assert_eq!(version_date(5), None);
}

#[test]
fn the_manifest_is_judged_against_the_floor_the_date_and_the_schema() {
    let t = today();
    let newer = baseline_version() + 100;
    // Newer than the floor.
    assert_eq!(
        judge_manifest(&manifest(newer), &state(0, 0), 30, t),
        Ok(true)
    );
    // At or below the baseline, the applied version, or the declined version.
    assert_eq!(
        judge_manifest(&manifest(baseline_version()), &state(0, 0), 30, t),
        Ok(false)
    );
    assert_eq!(
        judge_manifest(&manifest(newer), &state(newer, 0), 30, t),
        Ok(false)
    );
    assert_eq!(
        judge_manifest(&manifest(newer), &state(0, newer), 30, t),
        Ok(false)
    );
    // A date more than two days ahead is refused.
    let far = 2026102301u64;
    let e = judge_manifest(&manifest(far), &state(0, 0), 30, t).unwrap_err();
    assert_eq!(e.code, codes::ERROR_VERSION_TOO_FAR_AHEAD);
    assert!(judge_manifest(&manifest(2026102201), &state(0, 0), 30, t).is_ok());
    // A catalog that needs newer columns than this database has.
    let e = judge_manifest(&manifest(newer), &state(0, 0), 29, t).unwrap_err();
    assert_eq!(e.code, codes::ERROR_NEEDS_NEWER_APP);
    // Format, asset name, size and hash shape.
    let mut m = manifest(newer);
    m.format = 2;
    assert_eq!(
        judge_manifest(&m, &state(0, 0), 30, t).unwrap_err().code,
        codes::ERROR_MANIFEST_INVALID
    );
    let mut m = manifest(newer);
    m.asset_name = "../other.json".to_owned();
    assert!(judge_manifest(&m, &state(0, 0), 30, t).is_err());
    let mut m = manifest(newer);
    m.size = MAX_CATALOG_BYTES + 1;
    assert!(judge_manifest(&m, &state(0, 0), 30, t).is_err());
    let mut m = manifest(newer);
    m.sha256 = "xyz".to_owned();
    assert!(judge_manifest(&m, &state(0, 0), 30, t).is_err());
}

// ---------------------------------------------------------------------------
// Signature and key handling (fail-closed branches that need no signed file)
// ---------------------------------------------------------------------------

#[test]
fn no_compiled_in_key_means_no_trust() {
    assert!(
        TRUSTED_PUBLIC_KEYS.is_empty(),
        "the production key list stays empty until Jason supplies the public key"
    );
    assert_eq!(
        verify_manifest(&[], b"{}", "untrusted comment: x\nAAAA\n"),
        Err(VerifyError::NoTrustedKey)
    );
}

#[test]
fn a_garbage_signature_or_key_never_verifies() {
    assert_eq!(
        verify_manifest(
            &["RWQ0000000000000000000000000000000000000000000000000"],
            b"{}",
            "garbage"
        ),
        Err(VerifyError::BadSignature)
    );
    assert_eq!(
        verify_manifest(
            &["not a key"],
            b"{}",
            "untrusted comment: x\nAAAA\ntrusted comment: y\nAAAA\n"
        ),
        Err(VerifyError::BadSignature)
    );
}

// ---------------------------------------------------------------------------
// Staging
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_check_without_a_trusted_key_makes_no_request() {
    let server = crate::providers::loopback_test_server::LoopbackServer::start(vec![
        crate::providers::loopback_test_server::Canned::bytes(200, b"{}".to_vec()),
    ]);
    let dir = tempfile::tempdir().unwrap();
    let fetcher = crate::pinned_asset::PinnedFetcher::for_loopback_tests(&["127.0.0.1"]);
    let err = check_for_update(
        &fetcher,
        &server.base(),
        &[],
        &state(0, 0),
        30,
        dir.path(),
        today(),
    )
    .await
    .unwrap_err();
    assert_eq!(err.code, codes::ERROR_NO_TRUSTED_KEY);
    assert_eq!(server.connections(), 0, "fails closed before any traffic");
}

#[tokio::test]
async fn staged_files_are_looked_up_by_hash_only() {
    let dir = tempfile::tempdir().unwrap();
    for bad in ["", "abc", "../../etc", &"g".repeat(64), &"a".repeat(65)] {
        assert_eq!(
            load_staged(&["x"], dir.path(), bad).await.unwrap_err().code,
            codes::ERROR_NOT_STAGED,
            "{bad:?}"
        );
    }
    assert_eq!(
        load_staged(&["x"], dir.path(), &"a".repeat(64))
            .await
            .unwrap_err()
            .code,
        codes::ERROR_NOT_STAGED
    );
}

// ---------------------------------------------------------------------------
// Reconcile after a release migration
// ---------------------------------------------------------------------------

#[tokio::test]
async fn reconcile_forgets_a_catalog_the_release_has_overtaken() {
    with_db(|pool| async move {
        seed(&pool).await;
        let d = doc(json!({"catalog_version": baseline_version(), "models":[{
            "provider_id": HOST_B, "model_id": "only",
            "pricing": price("0.6", "1.6", "2026-10-15")}]}));
        // Recorded directly: apply_staged would refuse a version at the floor.
        let mut tx = pool.begin().await.unwrap();
        let out = apply_catalog(&mut tx, &d, today()).await.unwrap();
        store::record_apply(
            &mut tx,
            baseline_version(),
            &"0".repeat(64),
            &serde_json::to_string(&json!({"format":1,"catalog_version":baseline_version()}))
                .unwrap(),
            &serde_json::to_string(&out.undo).unwrap(),
            "2026-10-20T00:00:00Z",
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
        assert_eq!(
            reconcile_after_migration(&pool, today()).await.unwrap(),
            Reconciled::Superseded
        );
        let mut conn = pool.acquire().await.unwrap();
        assert_eq!(
            store::get_state(&mut conn).await.unwrap().applied_version,
            0
        );
        assert!(store::latest_history(&mut conn).await.unwrap().is_none());
    })
    .await;
}

#[tokio::test]
async fn reconcile_applies_a_newer_catalog_again_over_a_migration() {
    with_db(|pool| async move {
        seed(&pool).await;
        assert_eq!(
            reconcile_after_migration(&pool, today()).await.unwrap(),
            Reconciled::Nothing
        );
        // A real catalog document so the stored text can be parsed again.
        let text = json!({"format":1,"catalog_version":2026101501u64,"models":[{
            "provider_id": HOST_B, "model_id": "only",
            "pricing": price("0.6", "1.6", "2026-10-15")}]})
        .to_string();
        let d = parse_catalog(text.as_bytes()).unwrap();
        let mut tx = pool.begin().await.unwrap();
        let out = apply_catalog(&mut tx, &d, today()).await.unwrap();
        store::record_apply(
            &mut tx,
            2026101501,
            &"0".repeat(64),
            &text,
            &serde_json::to_string(&out.undo).unwrap(),
            "2026-10-20T00:00:00Z",
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
        let id = format!("{HOST_B}:only");
        let applied_price = col_text(&pool, "provider_models", &id, "pricing").await;

        // Reconcile with nothing disturbed: a no-op.
        assert_eq!(
            reconcile_after_migration(&pool, today()).await.unwrap(),
            Reconciled::Reapplied { changed: 0 }
        );
        // A release migration rewrites the row (older as-of, so the catalog's
        // own newer price must win again).
        sqlx::query("UPDATE provider_models SET pricing = ? WHERE id = ?")
            .bind(PAID)
            .bind(&id)
            .execute(&pool)
            .await
            .unwrap();
        let r = reconcile_after_migration(&pool, today()).await.unwrap();
        assert_eq!(r, Reconciled::Reapplied { changed: 1 });
        assert_eq!(
            col_text(&pool, "provider_models", &id, "pricing").await,
            applied_price
        );

        // A release migration that is NEWER than the catalog is not overwritten.
        let newer = r#"{"input_per_mtok":7,"output_per_mtok":8,"currency":"USD","cache":null,"tiers":null,"as_of":"2026-10-19","source_url":null,"source":"curated"}"#;
        sqlx::query("UPDATE provider_models SET pricing = ? WHERE id = ?")
            .bind(newer)
            .bind(&id)
            .execute(&pool)
            .await
            .unwrap();
        let r = reconcile_after_migration(&pool, today()).await.unwrap();
        assert_eq!(r, Reconciled::Reapplied { changed: 0 });
        assert_eq!(
            col_text(&pool, "provider_models", &id, "pricing").await,
            Some(newer.to_owned())
        );
    })
    .await;
}

// ---------------------------------------------------------------------------
// The baseline catalog
// ---------------------------------------------------------------------------

#[test]
fn the_baseline_catalog_is_a_valid_document_with_a_dated_version() {
    let d = parse_catalog(BASELINE_CATALOG_JSON.as_bytes()).expect("baseline parses");
    assert_eq!(d.format, FORMAT_VERSION);
    assert_eq!(d.catalog_version, baseline_version());
    assert!(version_date(baseline_version()).is_some());
    assert!(
        d.new_models.is_empty(),
        "a baseline lists existing rows only"
    );
}

#[tokio::test]
async fn the_baseline_catalog_agrees_with_the_migrated_database() {
    with_db(|pool| async move {
        let d = parse_catalog(BASELINE_CATALOG_JSON.as_bytes()).unwrap();
        let before = dump(&pool).await;
        let out = run(&pool, &d).await.expect("baseline applies");
        assert!(out.skipped.is_empty(), "{:?}", out.skipped);
        assert!(
            out.is_noop(),
            "the checked-in catalog must equal the migrated rows; regenerate it \
             with `cargo test dump_baseline_catalog -- --ignored --nocapture` \
             after a migration touches a governed column. Differences: {:?}",
            out.changes
        );
        assert_eq!(dump(&pool).await, before);
    })
    .await;
}

/// Regenerates src-tauri/catalog/provider-catalog.json from a freshly migrated
/// database: `cargo test dump_baseline_catalog -- --ignored --nocapture`, copy
/// the text between the markers into the file, and bump `catalog_version`.
#[tokio::test]
#[ignore]
async fn dump_baseline_catalog() {
    with_db(|pool| async move {
        fn num(v: &Value) -> String {
            v.to_string()
        }
        let mut providers = Vec::new();
        let mut models = Vec::new();
        let rows = sqlx::query(
            "SELECT id, free_tier, plan_inclusion, last_reviewed_at, review_trigger_note,
                    qr_recommended, qr_recommendation_rank, spend_limit_url,
                    credential_acquisition
               FROM providers WHERE is_local = 0 AND api_dialect IS NOT NULL ORDER BY id",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        for r in rows {
            let id: String = r.get("id");
            let mut p = serde_json::Map::new();
            p.insert("id".into(), json!(id));
            if let Some(t) = r.get::<Option<String>, _>("free_tier") {
                p.insert("free_tier".into(), serde_json::from_str(&t).unwrap());
            }
            if let Some(t) = r.get::<Option<String>, _>("plan_inclusion") {
                p.insert("plan_inclusion".into(), serde_json::from_str(&t).unwrap());
            }
            if let Some(t) = r.get::<Option<String>, _>("last_reviewed_at") {
                p.insert("last_reviewed_at".into(), json!(t));
            }
            if let Some(t) = r.get::<Option<String>, _>("review_trigger_note") {
                p.insert("review_trigger_note".into(), json!(t));
            }
            p.insert(
                "qr_recommended".into(),
                json!(r.get::<i64, _>("qr_recommended") != 0),
            );
            if let Some(k) = r.get::<Option<i64>, _>("qr_recommendation_rank") {
                p.insert("qr_recommendation_rank".into(), json!(k));
            }
            if let Some(t) = r.get::<Option<String>, _>("spend_limit_url") {
                p.insert("spend_limit_url".into(), json!(t));
            }
            if let Some(t) = r.get::<Option<String>, _>("credential_acquisition") {
                let v: Value = serde_json::from_str(&t).unwrap();
                if let Some(u) = v.get("keys_page_url").and_then(Value::as_str) {
                    p.insert("keys_page_url".into(), json!(u));
                }
            }
            providers.push(Value::Object(p));

            let ms = sqlx::query(
                "SELECT model_id, pricing, context_window_tokens, context_window_as_of,
                        request_options, token_factor, output_overhead, qr_recommended,
                        qr_recommendation_rank, requires_paid, is_default, status, retires_at
                   FROM provider_models WHERE provider_id = ? ORDER BY model_id",
            )
            .bind(&id)
            .fetch_all(&pool)
            .await
            .unwrap();
            for m in ms {
                let mut o = serde_json::Map::new();
                o.insert("provider_id".into(), json!(id));
                o.insert("model_id".into(), json!(m.get::<String, _>("model_id")));
                if let Some(t) = m.get::<Option<String>, _>("pricing") {
                    let v: Value = serde_json::from_str(&t).unwrap();
                    let tiers: Vec<Value> = v
                        .get("tiers")
                        .and_then(Value::as_array)
                        .map(|a| {
                            a.iter()
                                .map(|t| {
                                    let mut x = serde_json::Map::new();
                                    for k in ["prompt_tokens_up_to", "prompt_tokens_over"] {
                                        if let Some(n) = t.get(k) {
                                            x.insert(k.into(), n.clone());
                                        }
                                    }
                                    x.insert(
                                        "input_per_mtok".into(),
                                        json!(num(&t["input_per_mtok"])),
                                    );
                                    x.insert(
                                        "output_per_mtok".into(),
                                        json!(num(&t["output_per_mtok"])),
                                    );
                                    Value::Object(x)
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    let mut pr = serde_json::Map::new();
                    pr.insert("input_per_mtok".into(), json!(num(&v["input_per_mtok"])));
                    pr.insert("output_per_mtok".into(), json!(num(&v["output_per_mtok"])));
                    if !tiers.is_empty() {
                        pr.insert("tiers".into(), Value::Array(tiers));
                    }
                    pr.insert("as_of".into(), v["as_of"].clone());
                    if let Some(u) = v.get("source_url").filter(|u| !u.is_null()) {
                        pr.insert("source_url".into(), u.clone());
                    }
                    if let Some(n) = v.get("note") {
                        pr.insert("note".into(), n.clone());
                    }
                    o.insert("pricing".into(), Value::Object(pr));
                }
                if let Some(a) = m.get::<Option<String>, _>("context_window_as_of") {
                    o.insert(
                        "context_window".into(),
                        json!({"tokens": m.get::<i64, _>("context_window_tokens"), "as_of": a}),
                    );
                }
                o.insert(
                    "token_factor".into(),
                    json!(m.get::<f64, _>("token_factor")),
                );
                o.insert(
                    "output_overhead".into(),
                    json!(m.get::<f64, _>("output_overhead")),
                );
                if let Some(t) = m.get::<Option<String>, _>("request_options") {
                    let v: Value = serde_json::from_str(&t).unwrap();
                    for k in ["max_tokens", "max_completion_tokens"] {
                        if let Some(n) = v.get(k) {
                            o.insert("output_limit_tokens".into(), n.clone());
                        }
                    }
                }
                o.insert(
                    "qr_recommended".into(),
                    json!(m.get::<i64, _>("qr_recommended") != 0),
                );
                if let Some(k) = m.get::<Option<i64>, _>("qr_recommendation_rank") {
                    o.insert("qr_recommendation_rank".into(), json!(k));
                }
                o.insert(
                    "requires_paid".into(),
                    json!(m.get::<i64, _>("requires_paid") != 0),
                );
                o.insert(
                    "is_default".into(),
                    json!(m.get::<i64, _>("is_default") != 0),
                );
                o.insert("status".into(), json!(m.get::<String, _>("status")));
                if let Some(r) = m.get::<Option<String>, _>("retires_at") {
                    o.insert("retires_at".into(), json!(r));
                }
                models.push(Value::Object(o));
            }
        }
        let catalog = json!({
            "format": 1,
            "catalog_version": baseline_version(),
            "providers": providers,
            "models": models
        });
        println!("-----BEGIN BASELINE-----");
        println!("{}", serde_json::to_string_pretty(&catalog).unwrap());
        println!("-----END BASELINE-----");
    })
    .await;
}

// ---------------------------------------------------------------------------
// Codes
// ---------------------------------------------------------------------------

#[test]
fn the_code_set_is_pinned_unique_and_prefixed() {
    let mut seen = std::collections::HashSet::new();
    for c in codes::ALL {
        assert!(c.starts_with("catalog."), "{c}");
        assert!(seen.insert(*c), "duplicate code {c}");
    }
    assert_eq!(codes::ALL.len(), 36);
}

// ---------------------------------------------------------------------------
// Network discipline
// ---------------------------------------------------------------------------

fn rust_sources(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
    for e in std::fs::read_dir(dir).unwrap().flatten() {
        let p = e.path();
        if p.is_dir() {
            rust_sources(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
}

#[test]
fn the_catalog_network_calls_are_made_only_by_the_click_command() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    rust_sources(&root, &mut files);
    let mut fetch_callers = Vec::new();
    let mut check_callers = Vec::new();
    for f in files {
        let rel = f
            .strip_prefix(&root)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        if rel.ends_with("tests.rs") || rel.contains("/tests/") {
            continue;
        }
        let text = std::fs::read_to_string(&f).unwrap();
        // Strip the module's own definition lines and comments crudely: only
        // call sites matter, and a call is `.name(`.
        if text.contains(".fetch_small(") || text.contains(".fetch_to_file(") {
            fetch_callers.push(rel.clone());
        }
        if text.contains("check_for_update(") && !rel.starts_with("provider_catalog/") {
            check_callers.push(rel);
        }
    }
    fetch_callers.sort();
    assert_eq!(
        fetch_callers,
        vec!["provider_catalog/flow.rs".to_owned()],
        "only the catalog flow fetches (items.id=678 adds its own file here)"
    );
    assert_eq!(
        check_callers,
        vec!["commands/provider_catalog.rs".to_owned()],
        "only the click command checks for an update; nothing runs at startup"
    );
}

// ---------------------------------------------------------------------------
// Signature-positive tests over the committed TEST-ONLY fixtures
// ---------------------------------------------------------------------------
//
// src/provider_catalog/test_fixtures/ holds two throwaway minisign PUBLIC keys
// (A and B), a catalog body, its manifest, and the manifest signed by each key.
// The secret keys were generated in a temporary directory and destroyed; see
// test_fixtures/README-TEST-ONLY.txt. Nothing here is a release artifact and
// the production key list stays empty.

mod signed {
    use super::*;
    use crate::pinned_asset::{hex32, sha256_of, PinnedFetcher};
    use crate::providers::loopback_test_server::{Canned, LoopbackServer};

    const MANIFEST: &[u8] = include_bytes!("test_fixtures/catalog-manifest.json");
    const SIG_A: &str = include_str!("test_fixtures/catalog-manifest.json.minisig");
    const SIG_B: &str = include_str!("test_fixtures/catalog-manifest.json.keyb.minisig");
    const BODY: &[u8] = include_bytes!("test_fixtures/provider-catalog.json");
    const PUB_A: &str = include_str!("test_fixtures/test_key_a.pub");
    const PUB_B: &str = include_str!("test_fixtures/test_key_b.pub");

    fn fetcher() -> PinnedFetcher {
        PinnedFetcher::for_loopback_tests(&["127.0.0.1"])
    }

    fn flip(bytes: &[u8], at: usize) -> Vec<u8> {
        let mut v = bytes.to_vec();
        v[at] ^= 0x01;
        v
    }

    /// manifest, signature, body: the order check_for_update requests them.
    fn serve(manifest: &[u8], sig: &str, body: &[u8]) -> LoopbackServer {
        LoopbackServer::start(vec![
            Canned::bytes(200, manifest.to_vec()),
            Canned::bytes(200, sig.as_bytes().to_vec()),
            Canned::bytes(200, body.to_vec()),
        ])
    }

    fn body_sha() -> String {
        hex32(&sha256_of(BODY))
    }

    // -- the fixtures are what they claim ------------------------------------

    #[test]
    fn the_fixtures_agree_with_each_other() {
        let m = parse_manifest(MANIFEST).expect("manifest parses");
        assert_eq!(m.sha256, body_sha());
        assert_eq!(m.size, BODY.len() as u64);
        assert_eq!(m.asset_name, CATALOG_ASSET_NAME);
        assert_eq!(m.format, FORMAT_VERSION);
        let d = parse_catalog(BODY).expect("body parses");
        assert_eq!(d.catalog_version, m.catalog_version);
        assert!(m.catalog_version > baseline_version());
        assert!(PUB_A.contains("TEST ONLY") && PUB_B.contains("TEST ONLY"));
        assert_ne!(PUB_A, PUB_B);
    }

    #[test]
    fn no_secret_key_and_no_production_key_in_the_repository_fixtures() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src/provider_catalog/test_fixtures");
        for e in std::fs::read_dir(dir).unwrap().flatten() {
            let text = std::fs::read_to_string(e.path()).unwrap_or_default();
            // minisign writes this header into every secret key file.
            assert!(
                !text.contains("minisign encrypted secret key"),
                "{:?} looks like a secret key",
                e.file_name()
            );
            assert_ne!(
                e.path().extension().and_then(|x| x.to_str()),
                Some("key"),
                "{:?} must not be a key file",
                e.file_name()
            );
        }
        // The compiled-in list is empty, so it cannot contain a test key.
        assert!(TRUSTED_PUBLIC_KEYS.is_empty());
        for k in TRUSTED_PUBLIC_KEYS {
            let b64 = |s: &str| s.lines().nth(1).unwrap_or_default().to_owned();
            assert_ne!(*k, b64(PUB_A));
            assert_ne!(*k, b64(PUB_B));
        }
    }

    // -- verify_manifest --------------------------------------------------------

    #[test]
    fn a_valid_signature_is_accepted() {
        assert_eq!(verify_manifest(&[PUB_A], MANIFEST, SIG_A), Ok(()));
        assert_eq!(verify_manifest(&[PUB_B], MANIFEST, SIG_B), Ok(()));
        // The bare base64 line of a .pub file is accepted as well.
        let line = PUB_A.lines().nth(1).unwrap();
        assert_eq!(verify_manifest(&[line], MANIFEST, SIG_A), Ok(()));
    }

    #[test]
    fn a_tampered_manifest_is_rejected() {
        for at in [0, 20, MANIFEST.len() / 2, MANIFEST.len() - 1] {
            assert_eq!(
                verify_manifest(&[PUB_A], &flip(MANIFEST, at), SIG_A),
                Err(VerifyError::BadSignature),
                "byte {at}"
            );
        }
        let mut longer = MANIFEST.to_vec();
        longer.push(b'\n');
        assert_eq!(
            verify_manifest(&[PUB_A], &longer, SIG_A),
            Err(VerifyError::BadSignature)
        );
        let mut shorter = MANIFEST.to_vec();
        shorter.pop();
        assert_eq!(
            verify_manifest(&[PUB_A], &shorter, SIG_A),
            Err(VerifyError::BadSignature)
        );
    }

    #[test]
    fn a_tampered_signature_is_rejected() {
        // Damage the signature line, then the trusted comment, then truncate.
        let lines: Vec<&str> = SIG_A.lines().collect();
        let mut sig_line = lines[1].to_owned();
        let c = if sig_line.starts_with('A') { "B" } else { "A" };
        sig_line.replace_range(20..21, c);
        let damaged = format!("{}\n{}\n{}\n{}\n", lines[0], sig_line, lines[2], lines[3]);
        assert_eq!(
            verify_manifest(&[PUB_A], MANIFEST, &damaged),
            Err(VerifyError::BadSignature)
        );
        let comment = format!(
            "{}\n{}\n{}\n{}\n",
            lines[0], lines[1], "trusted comment: forged", lines[3]
        );
        assert_eq!(
            verify_manifest(&[PUB_A], MANIFEST, &comment),
            Err(VerifyError::BadSignature)
        );
        assert_eq!(
            verify_manifest(&[PUB_A], MANIFEST, &SIG_A[..SIG_A.len() / 2]),
            Err(VerifyError::BadSignature)
        );
        assert_eq!(
            verify_manifest(&[PUB_A], MANIFEST, ""),
            Err(VerifyError::BadSignature)
        );
    }

    #[test]
    fn the_wrong_key_is_rejected() {
        assert_eq!(
            verify_manifest(&[PUB_B], MANIFEST, SIG_A),
            Err(VerifyError::BadSignature)
        );
        assert_eq!(
            verify_manifest(&[PUB_A], MANIFEST, SIG_B),
            Err(VerifyError::BadSignature)
        );
    }

    #[test]
    fn two_keys_support_rotation() {
        // Either key in either order verifies its own signature.
        for keys in [[PUB_A, PUB_B], [PUB_B, PUB_A]] {
            assert_eq!(verify_manifest(&keys, MANIFEST, SIG_A), Ok(()));
            assert_eq!(verify_manifest(&keys, MANIFEST, SIG_B), Ok(()));
        }
        // After the old key is rotated out, its signatures stop verifying.
        assert_eq!(
            verify_manifest(&[PUB_B], MANIFEST, SIG_A),
            Err(VerifyError::BadSignature)
        );
        assert_eq!(verify_manifest(&[PUB_B], MANIFEST, SIG_B), Ok(()));
        // A malformed key in the list is skipped, never trusted.
        assert_eq!(
            verify_manifest(&["not a key", PUB_A], MANIFEST, SIG_A),
            Ok(())
        );
        assert_eq!(
            verify_manifest(&["not a key"], MANIFEST, SIG_A),
            Err(VerifyError::BadSignature)
        );
    }

    // -- check -> preview -> apply -> rollback over loopback --------------------

    #[tokio::test]
    async fn the_full_flow_checks_previews_applies_and_rolls_back() {
        with_db(|pool| async move {
            seed(&pool).await;
            let original = dump(&pool).await;
            let server = serve(MANIFEST, SIG_A, BODY);
            let stage = tempfile::tempdir().unwrap();

            // 1. check: fetch, verify, pin, stage.
            let result = check_for_update(
                &fetcher(),
                &server.base(),
                &[PUB_A],
                &state(0, 0),
                30,
                stage.path(),
                today(),
            )
            .await
            .expect("a correctly signed newer catalog is available");
            let CheckResult::Available(staged) = result else {
                panic!("expected an available update");
            };
            assert_eq!(staged.sha256, body_sha());
            assert_eq!(staged.manifest.catalog_version, 2026101501);
            assert_eq!(server.connections(), 3);
            let paths: Vec<String> = server.captured().iter().map(|c| c.path.clone()).collect();
            assert_eq!(
                paths,
                vec![
                    "/catalog-manifest.json".to_owned(),
                    "/catalog-manifest.json.minisig".to_owned(),
                    "/provider-catalog.json".to_owned()
                ]
            );
            for req in server.captured() {
                assert!(req.header("user-agent").is_none(), "no identifying header");
            }
            let dir = stage.path().join(body_sha());
            for f in ["manifest.json", "manifest.minisig", "provider-catalog.json"] {
                assert!(dir.join(f).exists(), "{f} is staged");
            }
            assert_eq!(dump(&pool).await, original, "a check writes no catalog row");

            // 2. preview: what changes, and who is asked again. No write.
            let outcome = preview(&pool, &staged.doc, today()).await.unwrap();
            let mut reasons = effect_reasons(&outcome, HOST_A);
            reasons.sort_unstable();
            assert_eq!(
                reasons,
                vec![
                    codes::CONSENT_COST_CLASS_CHANGED,
                    codes::CONSENT_MODEL_ADDED
                ]
            );
            assert_eq!(dump(&pool).await, original);

            // 3. apply the staged files after re-verifying them.
            let again = load_staged(&[PUB_A], stage.path(), &staged.sha256)
                .await
                .expect("staged files re-verify");
            let outcome = apply_staged(&pool, &again, today(), now()).await.unwrap();
            assert!(!outcome.is_noop());
            let m4 = format!("{HOST_A}:model-4");
            assert_eq!(
                col_text(&pool, "provider_models", &m4, "request_options").await,
                Some(r#"{"max_tokens":700}"#.to_owned())
            );
            let m2 = format!("{HOST_A}:model-2");
            assert!(col_text(&pool, "provider_models", &m2, "pricing")
                .await
                .unwrap()
                .contains("\"input_per_mtok\":0.10"));
            let mut conn = pool.acquire().await.unwrap();
            let st = store::get_state(&mut conn).await.unwrap();
            assert_eq!(st.applied_version, 2026101501);
            assert_eq!(st.applied_sha256.as_deref(), Some(body_sha().as_str()));
            drop(conn);

            // 4. the same catalog is not offered again.
            let server2 = LoopbackServer::start(vec![
                Canned::bytes(200, MANIFEST.to_vec()),
                Canned::bytes(200, SIG_A.as_bytes().to_vec()),
            ]);
            let stage2 = tempfile::tempdir().unwrap();
            let again = check_for_update(
                &fetcher(),
                &server2.base(),
                &[PUB_A],
                &state(2026101501, 0),
                30,
                stage2.path(),
                today(),
            )
            .await
            .unwrap();
            assert!(matches!(again, CheckResult::UpToDate));
            assert_eq!(server2.connections(), 2, "the body is not fetched");

            // 5. roll back to exactly the original rows.
            rollback_latest(&pool, now()).await.unwrap();
            assert_eq!(dump(&pool).await, original);
        })
        .await;
    }

    #[tokio::test]
    async fn a_catalog_signed_by_the_second_key_is_accepted_during_rotation() {
        let server = serve(MANIFEST, SIG_B, BODY);
        let stage = tempfile::tempdir().unwrap();
        let r = check_for_update(
            &fetcher(),
            &server.base(),
            &[PUB_A, PUB_B],
            &state(0, 0),
            30,
            stage.path(),
            today(),
        )
        .await
        .unwrap();
        assert!(matches!(r, CheckResult::Available(_)));
    }

    #[tokio::test]
    async fn the_wrong_key_stops_the_check_before_the_body_is_fetched() {
        let server = serve(MANIFEST, SIG_A, BODY);
        let stage = tempfile::tempdir().unwrap();
        let err = check_for_update(
            &fetcher(),
            &server.base(),
            &[PUB_B],
            &state(0, 0),
            30,
            stage.path(),
            today(),
        )
        .await
        .unwrap_err();
        assert_eq!(err.code, codes::ERROR_SIGNATURE_INVALID);
        assert_eq!(server.connections(), 2, "manifest and signature only");
        assert!(!stage.path().join(body_sha()).exists());
    }

    #[tokio::test]
    async fn a_tampered_manifest_is_refused_before_the_body_is_fetched() {
        // The attacker raises the size and keeps the old signature.
        let text = String::from_utf8(MANIFEST.to_vec()).unwrap();
        let forged = text.replacen("\"size\":", "\"size\":9", 1);
        let server = serve(forged.as_bytes(), SIG_A, BODY);
        let stage = tempfile::tempdir().unwrap();
        let err = check_for_update(
            &fetcher(),
            &server.base(),
            &[PUB_A],
            &state(0, 0),
            30,
            stage.path(),
            today(),
        )
        .await
        .unwrap_err();
        assert_eq!(err.code, codes::ERROR_SIGNATURE_INVALID);
        assert_eq!(server.connections(), 2);
    }

    #[tokio::test]
    async fn a_tampered_body_fails_the_pinned_hash_and_leaves_nothing_staged() {
        with_db(|pool| async move {
            seed(&pool).await;
            let original = dump(&pool).await;
            // Same length, one byte different: only the hash can catch it.
            let server = serve(MANIFEST, SIG_A, &flip(BODY, 100));
            let stage = tempfile::tempdir().unwrap();
            let err = check_for_update(
                &fetcher(),
                &server.base(),
                &[PUB_A],
                &state(0, 0),
                30,
                stage.path(),
                today(),
            )
            .await
            .unwrap_err();
            assert_eq!(err.code, codes::ERROR_ASSET_MISMATCH);
            let dir = stage.path().join(body_sha());
            assert!(!dir.join("provider-catalog.json").exists());
            assert!(!dir.join("provider-catalog.json.part").exists());
            assert!(!dir.join("manifest.json").exists());
            assert_eq!(dump(&pool).await, original);
            let mut conn = pool.acquire().await.unwrap();
            assert_eq!(
                store::get_state(&mut conn).await.unwrap().applied_version,
                0
            );
        })
        .await;
    }

    #[tokio::test]
    async fn a_body_of_the_wrong_length_is_refused() {
        let mut longer = BODY.to_vec();
        longer.extend_from_slice(b" ");
        let server = serve(MANIFEST, SIG_A, &longer);
        let stage = tempfile::tempdir().unwrap();
        let err = check_for_update(
            &fetcher(),
            &server.base(),
            &[PUB_A],
            &state(0, 0),
            30,
            stage.path(),
            today(),
        )
        .await
        .unwrap_err();
        assert_eq!(err.code, codes::ERROR_ASSET_MISMATCH);
    }

    #[tokio::test]
    async fn a_signed_manifest_is_still_judged_on_date_and_schema() {
        let stage = tempfile::tempdir().unwrap();
        // The catalog is dated 2026-10-15; "today" five days earlier is too far.
        let server = serve(MANIFEST, SIG_A, BODY);
        let early = NaiveDate::from_ymd_opt(2026, 10, 10).unwrap();
        let err = check_for_update(
            &fetcher(),
            &server.base(),
            &[PUB_A],
            &state(0, 0),
            30,
            stage.path(),
            early,
        )
        .await
        .unwrap_err();
        assert_eq!(err.code, codes::ERROR_VERSION_TOO_FAR_AHEAD);
        // A database without the columns the manifest needs.
        let server = serve(MANIFEST, SIG_A, BODY);
        let err = check_for_update(
            &fetcher(),
            &server.base(),
            &[PUB_A],
            &state(0, 0),
            29,
            stage.path(),
            today(),
        )
        .await
        .unwrap_err();
        assert_eq!(err.code, codes::ERROR_NEEDS_NEWER_APP);
        // A rolled-back version is not offered again.
        let server = serve(MANIFEST, SIG_A, BODY);
        let r = check_for_update(
            &fetcher(),
            &server.base(),
            &[PUB_A],
            &state(0, 2026101501),
            30,
            stage.path(),
            today(),
        )
        .await
        .unwrap();
        assert!(matches!(r, CheckResult::UpToDate));
    }

    // -- the staged files are not trusted on disk ---------------------------------

    async fn staged_dir() -> (tempfile::TempDir, std::path::PathBuf, LoopbackServer) {
        let server = serve(MANIFEST, SIG_A, BODY);
        let stage = tempfile::tempdir().unwrap();
        check_for_update(
            &fetcher(),
            &server.base(),
            &[PUB_A],
            &state(0, 0),
            30,
            stage.path(),
            today(),
        )
        .await
        .unwrap();
        let dir = stage.path().join(body_sha());
        (stage, dir, server)
    }

    #[tokio::test]
    async fn a_staged_body_swapped_on_disk_is_refused_at_apply() {
        let (stage, dir, _server) = staged_dir().await;
        std::fs::write(dir.join("provider-catalog.json"), flip(BODY, 100)).unwrap();
        let err = load_staged(&[PUB_A], stage.path(), &body_sha())
            .await
            .unwrap_err();
        assert_eq!(err.code, codes::ERROR_ASSET_MISMATCH);
    }

    #[tokio::test]
    async fn a_staged_manifest_swapped_on_disk_is_refused_at_apply() {
        let (stage, dir, _server) = staged_dir().await;
        std::fs::write(dir.join("manifest.json"), flip(MANIFEST, 30)).unwrap();
        let err = load_staged(&[PUB_A], stage.path(), &body_sha())
            .await
            .unwrap_err();
        assert_eq!(err.code, codes::ERROR_SIGNATURE_INVALID);
    }

    #[tokio::test]
    async fn a_staged_signature_from_another_key_is_refused_at_apply() {
        let (stage, dir, _server) = staged_dir().await;
        std::fs::write(dir.join("manifest.minisig"), SIG_B).unwrap();
        // Only key A is trusted: key B's signature does not verify.
        let err = load_staged(&[PUB_A], stage.path(), &body_sha())
            .await
            .unwrap_err();
        assert_eq!(err.code, codes::ERROR_SIGNATURE_INVALID);
        // With both keys trusted (a rotation window) it does.
        assert!(load_staged(&[PUB_A, PUB_B], stage.path(), &body_sha())
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn with_no_trusted_key_apply_refuses_even_staged_signed_files() {
        let (stage, _dir, _server) = staged_dir().await;
        let err = load_staged(TRUSTED_PUBLIC_KEYS, stage.path(), &body_sha())
            .await
            .unwrap_err();
        assert_eq!(err.code, codes::ERROR_NO_TRUSTED_KEY);
    }
}
