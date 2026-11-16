// src-tauri/src/commands/run_plan_commands_tests.rs
//
// Tests of the run plan commands (items.id=901 stage 8) through their *_for_session bodies:
// real persona and shared.db files under a scratch data root, no Tauri state.

use serde_json::Value;

use super::*;
use crate::auth::registry::key_hex;
use crate::conductor::run_park::ParkRecord;
use crate::persistence::session_store::latch_write_suppressed;
use crate::spend::codes::Code;
use crate::test_support::ENV_MUTEX;

const CHAT: &str = "chat-plan";
const FOCUS: &str = "plan-fixture";
const PLAIN: &str = "plain-fixture";
const NO_SETTINGS: &str = "no-settings-fixture";
const MASTER_KEY: [u8; crate::auth::kdf::MASTER_KEY_LEN] =
    [0xABu8; crate::auth::kdf::MASTER_KEY_LEN];

struct Env {
    _tempdir: tempfile::TempDir,
    _lock: tokio::sync::MutexGuard<'static, ()>,
    saved_root: Option<String>,
    pool: sqlx::SqlitePool,
    scheduler: Arc<ConductorScheduler>,
    memory: PlanMemory,
    user: String,
    persona: String,
    key: String,
}

impl Drop for Env {
    fn drop(&mut self) {
        match &self.saved_root {
            Some(v) => std::env::set_var("QR_DATA_ROOT", v),
            None => std::env::remove_var("QR_DATA_ROOT"),
        }
    }
}

impl Env {
    fn plan_env(&self) -> PlanEnv<'_> {
        PlanEnv {
            pool: &self.pool,
            scheduler: Arc::clone(&self.scheduler),
            memory: &self.memory,
            user_id: &self.user,
            key_hex: &self.key,
        }
    }
}

async fn add_user(pool: &sqlx::SqlitePool, name: &str) -> String {
    let id = uuid::Uuid::new_v4().to_string();
    crate::auth::user_store::create_user(
        pool,
        &id,
        name,
        "user",
        false,
        &[0u8; crate::auth::kdf::SALT_LEN],
        crate::auth::kdf::DEFAULT_ARGON2_MEMORY_KIB,
        crate::auth::kdf::DEFAULT_ARGON2_ITERATIONS,
        crate::auth::kdf::DEFAULT_ARGON2_PARALLELISM,
        &[0u8; 32],
    )
    .await
    .expect("create_user");
    id
}

async fn settings_for(pool: &sqlx::SqlitePool, persona: &str, focus: &str) {
    crate::persistence::focus_settings_store::create_focus_settings(
        pool,
        persona,
        focus,
        "bidirectional",
        "shared",
        2,
        crate::conductor::tokens::ExternalAccess::LocalOnly,
        "open",
        None,
    )
    .await
    .expect("focus_settings row");
}

async fn env() -> Env {
    let lock = ENV_MUTEX.lock().await;
    let saved_root = std::env::var("QR_DATA_ROOT").ok();
    let tempdir = tempfile::tempdir().expect("tempdir");
    std::env::set_var("QR_DATA_ROOT", tempdir.path());
    crate::persistence::migrations::migrate_shared_db()
        .await
        .expect("shared.db");
    let pool =
        sqlx::SqlitePool::connect_with(crate::providers::utils::connect_options_unencrypted(
            &crate::providers::utils::db_path_shared(),
        ))
        .await
        .expect("pool");
    let key = key_hex(&MASTER_KEY);
    let user = add_user(&pool, "Plan User").await;
    let persona = uuid::Uuid::new_v4().to_string();
    persona_store::create_persona(&pool, &persona, "P", "personal", &user, &key, None)
        .await
        .expect("create_persona");

    // Every curated local model is installed, so the plan has candidates to offer.
    sqlx::query("UPDATE providers SET installed = 1 WHERE provider_type = 'local_model'")
        .execute(&pool)
        .await
        .unwrap();

    // A loadable Focus with one local model block (cb-12) and a preset document type.
    for (focus, name) in [
        (FOCUS, "Plan Fixture"),
        (PLAIN, "Plain Fixture"),
        (NO_SETTINGS, "No Settings"),
    ] {
        sqlx::query(
            "INSERT INTO focuses
                (focus_id, display_name, description, version, max_routing_tier,
                 output_type, suggest_in_focuses, multi_source_validation,
                 generic_title_template, status, created_at, updated_at)
             VALUES (?, ?, 'A fixture Focus.', '1.0', 'local_only', 'general',
                     '[]', 0, 'Hidden item', 'shipped', datetime('now'), datetime('now'))",
        )
        .bind(focus)
        .bind(name)
        .execute(&pool)
        .await
        .unwrap();
    }
    sqlx::query(
        "INSERT INTO focus_block_compositions
            (id, focus_id, action_id, block_stable_id, sequence_index, created_at, updated_at)
         VALUES ('pf:draft', ?, 'draft', 'cb-12', 1, datetime('now'), datetime('now'))",
    )
    .bind(FOCUS)
    .execute(&pool)
    .await
    .unwrap();
    // An earlier ordinary local step produces the sender details the block declares.
    sqlx::query(
        "INSERT INTO focus_block_compositions
            (id, focus_id, action_id, block_stable_id, sequence_index, created_at, updated_at)
         VALUES ('pf:sender', ?, 'sender', NULL, 0, datetime('now'), datetime('now'))",
    )
    .bind(FOCUS)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO focus_block_customization
            (id, composition_id, persona_id, customization, created_at, updated_at)
         VALUES ('pf:sender:d', 'pf:sender', NULL, ?, datetime('now'), datetime('now'))",
    )
    .bind(
        r#"{"routing_tier": 1, "task_type": "quick_response", "output_var": "sender_details",
            "prompt_template": "Say hello."}"#,
    )
    .execute(&pool)
    .await
    .unwrap();
    let configuration = serde_json::json!({
        "common": {
            "display_name": "Writing your letter",
            "output_var": "draft_output",
            "need": { "task_type": "creative_writing", "difficulty": "low", "output_size": "short" },
            "declared_inputs": ["content_brief", "sender_details"],
            "range": {
                "quality_floor": { "min": "basic", "max": "standard" },
                "highest_tier": "local_only",
                "destinations": ["local"]
            },
            "class_preference": null
        },
        "spec": {
            "document_type": "rma_letter",
            "brief_var": "content_brief",
            "sender_var": "sender_details",
            "context_var": null
        }
    });
    sqlx::query(
        "INSERT INTO focus_block_customization
            (id, composition_id, persona_id, customization, created_at, updated_at)
         VALUES ('pf:draft:d', 'pf:draft', NULL, ?, datetime('now'), datetime('now'))",
    )
    .bind(configuration.to_string())
    .execute(&pool)
    .await
    .unwrap();
    for sql in [
        "INSERT INTO focus_launch_questions
           (focus_id, question_id, slot, kind, sort_order, asked, default_option, prompt)
         VALUES ('plan-fixture', 'kind', 'document_type', 'choice', 0, 0, 'rma_letter', 'Which kind?')",
        "INSERT INTO focus_launch_options
           (focus_id, question_id, option_id, sort_order, label, hint)
         VALUES ('plan-fixture', 'kind', 'rma_letter', 0, 'RMA letter', '')",
    ] {
        sqlx::query(sql).execute(&pool).await.unwrap();
    }
    // A Focus with no model block: one ordinary local step.
    sqlx::query(
        "INSERT INTO focus_block_compositions
            (id, focus_id, action_id, block_stable_id, sequence_index, created_at, updated_at)
         VALUES ('lf:a', ?, 'a', NULL, 0, datetime('now'), datetime('now'))",
    )
    .bind(PLAIN)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO focus_block_customization
            (id, composition_id, persona_id, customization, created_at, updated_at)
         VALUES ('lf:a:d', 'lf:a', NULL, ?, datetime('now'), datetime('now'))",
    )
    .bind(
        r#"{"routing_tier": 1, "task_type": "quick_response", "output_var": "o",
            "prompt_template": "Say hello."}"#,
    )
    .execute(&pool)
    .await
    .unwrap();
    // NO_SETTINGS has no composition and no focus_settings row: it cannot be authorized.
    settings_for(&pool, &persona, FOCUS).await;
    settings_for(&pool, &persona, PLAIN).await;

    Env {
        _tempdir: tempdir,
        _lock: lock,
        saved_root,
        pool,
        scheduler: Arc::new(ConductorScheduler::new()),
        memory: PlanMemory::default(),
        user,
        persona,
        key,
    }
}

async fn plan(e: &Env, chat: Option<&str>) -> Result<RunPlanView, String> {
    get_run_plan_for_session(&e.plan_env(), &e.persona, chat, FOCUS).await
}

fn request(
    e: &Env,
    chat: Option<&str>,
    token: Option<&str>,
    digest: &str,
) -> ApproveRunPlanRequest {
    ApproveRunPlanRequest {
        persona_id: e.persona.clone(),
        context_key: chat.map(str::to_owned),
        plan_token: token.map(str::to_owned),
        focus_id: FOCUS.to_owned(),
        digest: digest.to_owned(),
        ceiling_micros: "0".to_owned(),
        choices: Vec::new(),
    }
}

async fn approve(e: &Env, r: &ApproveRunPlanRequest) -> Result<RunPlanApprovalView, String> {
    approve_run_plan_for_session(&e.plan_env(), r).await
}

async fn conn(e: &Env) -> sqlx::SqliteConnection {
    output_store::open_outputs_db_retrying(&e.user, &e.persona, &e.key)
        .await
        .unwrap()
}

async fn rows(e: &Env, table: &str) -> i64 {
    let mut c = conn(e).await;
    sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {table}"))
        .fetch_one(&mut c)
        .await
        .unwrap()
}

async fn latch(e: &Env, chat: &str) {
    let mut c = conn(e).await;
    latch_write_suppressed(&mut c, chat, FOCUS, "run-1")
        .await
        .unwrap();
}

fn unmeasured_pin(p: &RunPlanView) -> PinInput {
    let step = &p.steps[0];
    let offer = step
        .offers
        .iter()
        .find(|o| o.strategy == "local_unmeasured" && o.selectable)
        .expect("the fixture offers an unmeasured local model");
    PinInput {
        action_id: step.action_id.clone(),
        strategy: offer.strategy.clone(),
        provider_id: offer.provider_id.clone(),
        model_id: offer.model_id.clone(),
    }
}

// -- get_run_plan --------------------------------------------------------------------------

#[tokio::test]
async fn a_chat_gets_the_plan_of_its_model_block_focus_and_nothing_is_written() {
    let e = env().await;
    let p = plan(&e, Some(CHAT)).await.unwrap();
    assert_eq!(p.focus_id, FOCUS);
    assert_eq!(p.digest.len(), 64);
    assert!(!p.needs_approval, "a local-only range needs no approval");
    assert!(!p.approved);
    assert_eq!(p.plan_token, None);
    assert_eq!(p.ceiling_max_micros, "25000000");
    assert_eq!(p.estimate_micros, "0");
    assert_eq!(p.document_profile.as_deref(), Some("rma_letter"));
    assert_eq!(p.steps.len(), 1);
    let s = &p.steps[0];
    assert_eq!(s.action_id, "draft");
    assert_eq!(s.kind, "model_block");
    assert_eq!(s.required_bar, "basic");
    assert_eq!(s.worth_paying_bar, "standard");
    assert_eq!(s.destinations, vec!["local".to_owned()]);
    assert_eq!(s.highest_tier, "local_only");
    assert!(!s.hosted_possible);
    assert_eq!(s.answer.kind, "selected");
    assert_eq!(s.answer.subject.as_deref(), Some("local"));
    assert!(s
        .offers
        .iter()
        .any(|o| o.strategy == "local_unmeasured" && o.selectable));
    assert!(p.strategies.iter().any(|s| s.kind == "local" && s.listed));
    // Read only: no run row, no approval row, no park, no offer.
    for t in ["focus_runs", "plan_approvals", "run_parks"] {
        assert_eq!(rows(&e, t).await, 0, "{t}");
    }
    assert_eq!(e.memory.offer_count(), 0);
}

#[tokio::test]
async fn the_digest_is_the_one_the_run_recomputes() {
    let e = env().await;
    let a = plan(&e, Some(CHAT)).await.unwrap();
    let b = plan(&e, Some("another-chat")).await.unwrap();
    assert_eq!(a.digest, b.digest, "structure only: the same for any chat");
    // And approving it makes the run's own gate proceed with that approval.
    approve(&e, &request(&e, Some(CHAT), None, &a.digest))
        .await
        .unwrap();
    assert!(plan(&e, Some(CHAT)).await.unwrap().approved);
    assert!(!plan(&e, Some("another-chat")).await.unwrap().approved);
}

#[tokio::test]
async fn a_call_with_no_chat_registers_the_offer_and_returns_an_opaque_token() {
    let e = env().await;
    let p = plan(&e, None).await.unwrap();
    let token = p.plan_token.clone().expect("a token");
    assert_eq!(token.len(), 32);
    assert!(token.chars().all(|c| c.is_ascii_hexdigit()));
    assert_eq!(e.memory.offer_count(), 1);
    // Each call is its own offer, and the token is not derived from the plan.
    let q = plan(&e, None).await.unwrap();
    assert_ne!(q.plan_token.unwrap(), token);
    assert_eq!(p.digest, q.digest);
}

#[tokio::test]
async fn a_focus_with_no_model_block_has_no_plan() {
    let e = env().await;
    let r = get_run_plan_for_session(&e.plan_env(), &e.persona, Some(CHAT), PLAIN).await;
    assert_eq!(r.unwrap_err(), "no_plan");
    assert_eq!(e.memory.offer_count(), 0, "no offer for no plan");
}

// -- approve_run_plan ----------------------------------------------------------------------

#[tokio::test]
async fn an_unlatched_chat_approval_is_persisted_read_back_and_revoked() {
    let e = env().await;
    let p = plan(&e, Some(CHAT)).await.unwrap();
    let pin = unmeasured_pin(&p);
    let mut req = request(&e, Some(CHAT), None, &p.digest);
    req.choices = vec![pin.clone()];
    req.ceiling_micros = "1000".to_owned();
    let v = approve(&e, &req).await.unwrap();
    assert!(v.approved);
    assert_eq!(v.home.as_deref(), Some("persisted"));
    assert_eq!(v.ceiling_micros.as_deref(), Some("1000"));
    assert!(!v.one_time_chat);
    assert_eq!(v.plan_token, None);
    assert_eq!(v.choices.len(), 1);
    assert_eq!(v.choices[0].action_id, pin.action_id);
    assert_eq!(v.choices[0].strategy, "local_unmeasured");
    assert_eq!(rows(&e, "plan_approvals").await, 1);
    assert_eq!(e.memory.approval_count(), 0);

    // Read back through its own command, and through the plan.
    let got = get_run_plan_approval_for_session(&e.plan_env(), &e.persona, CHAT, FOCUS)
        .await
        .unwrap();
    assert_eq!(got.home.as_deref(), Some("persisted"));
    assert_eq!(got.digest, v.digest);
    assert_eq!(got.ceiling_micros.as_deref(), Some("1000"));
    assert_eq!(got.choices, v.choices);
    assert!(plan(&e, Some(CHAT)).await.unwrap().approved);

    // Approved a second time replaces it (one row).
    let again = approve(&e, &request(&e, Some(CHAT), None, &p.digest))
        .await
        .unwrap();
    assert!(again.choices.is_empty());
    assert_eq!(rows(&e, "plan_approvals").await, 1);

    // Revocation reaches the persisted home.
    let r =
        revoke_run_plan_approval_for_session(&e.plan_env(), &e.persona, Some(CHAT), None, FOCUS)
            .await
            .unwrap();
    assert_eq!(
        r,
        RunPlanRevocationView {
            memory: false,
            persisted: true
        }
    );
    let none = get_run_plan_approval_for_session(&e.plan_env(), &e.persona, CHAT, FOCUS)
        .await
        .unwrap();
    assert!(!none.approved && none.home.is_none() && none.digest.is_none());
    assert!(!plan(&e, Some(CHAT)).await.unwrap().approved);
}

#[tokio::test]
async fn a_latched_chat_approval_goes_to_memory_and_writes_no_row() {
    let e = env().await;
    latch(&e, CHAT).await;
    let p = plan(&e, Some(CHAT)).await.unwrap();
    let v = approve(&e, &request(&e, Some(CHAT), None, &p.digest))
        .await
        .unwrap();
    assert!(v.approved);
    assert_eq!(v.home.as_deref(), Some("memory"));
    assert!(v.one_time_chat);
    assert_eq!(rows(&e, "plan_approvals").await, 0, "nothing persisted");
    assert_eq!(e.memory.approval_count(), 1);

    let got = get_run_plan_approval_for_session(&e.plan_env(), &e.persona, CHAT, FOCUS)
        .await
        .unwrap();
    assert_eq!(got.home.as_deref(), Some("memory"));
    assert!(got.one_time_chat);
    assert!(
        plan(&e, Some(CHAT)).await.unwrap().approved,
        "the run reads it too"
    );

    // Revocation is not latch-gated and removes the memory home.
    let r =
        revoke_run_plan_approval_for_session(&e.plan_env(), &e.persona, Some(CHAT), None, FOCUS)
            .await
            .unwrap();
    assert_eq!(
        r,
        RunPlanRevocationView {
            memory: true,
            persisted: false
        }
    );
    assert!(!plan(&e, Some(CHAT)).await.unwrap().approved);
    assert_eq!(rows(&e, "plan_approvals").await, 0);
}

#[tokio::test]
async fn an_approval_persisted_before_a_latch_still_reads_and_is_revoked_after_it() {
    let e = env().await;
    let p = plan(&e, Some(CHAT)).await.unwrap();
    approve(&e, &request(&e, Some(CHAT), None, &p.digest))
        .await
        .unwrap();
    latch(&e, CHAT).await;
    let got = get_run_plan_approval_for_session(&e.plan_env(), &e.persona, CHAT, FOCUS)
        .await
        .unwrap();
    assert_eq!(got.home.as_deref(), Some("persisted"));
    assert!(got.one_time_chat);
    let r =
        revoke_run_plan_approval_for_session(&e.plan_env(), &e.persona, Some(CHAT), None, FOCUS)
            .await
            .unwrap();
    assert!(r.persisted, "not latch-gated");
    assert_eq!(rows(&e, "plan_approvals").await, 0);
}

#[tokio::test]
async fn a_stale_digest_is_plan_changed_and_approves_nothing() {
    let e = env().await;
    let p = plan(&e, Some(CHAT)).await.unwrap();
    let stale = "0".repeat(64);
    assert_ne!(stale, p.digest);
    let err = approve(&e, &request(&e, Some(CHAT), None, &stale))
        .await
        .unwrap_err();
    assert_eq!(err, "plan_changed");
    for junk in ["", "not a digest", &"x".repeat(500)] {
        assert_eq!(
            approve(&e, &request(&e, Some(CHAT), None, junk))
                .await
                .unwrap_err(),
            "plan_changed"
        );
    }
    assert_eq!(rows(&e, "plan_approvals").await, 0);
    assert_eq!(e.memory.approval_count(), 0);
}

#[tokio::test]
async fn a_chatless_run_is_approved_by_its_token_which_the_run_takes_once() {
    let e = env().await;
    let p = plan(&e, None).await.unwrap();
    let token = p.plan_token.clone().unwrap();
    let v = approve(&e, &request(&e, None, Some(&token), &p.digest))
        .await
        .unwrap();
    assert!(v.approved);
    assert_eq!(v.home.as_deref(), Some("memory"));
    assert_eq!(v.plan_token.as_deref(), Some(token.as_str()));
    assert!(!v.one_time_chat);
    assert_eq!(
        rows(&e, "plan_approvals").await,
        0,
        "a chatless approval is never persisted"
    );

    // The run (plan_gate) takes it once, bound to user, persona and Focus.
    let owner = OfferOwner {
        user_id: e.user.clone(),
        persona_id: e.persona.clone(),
        focus_id: FOCUS.to_owned(),
    };
    let other_focus = OfferOwner {
        focus_id: "other".to_owned(),
        ..owner.clone()
    };
    assert!(e.memory.take_offer_approval(&token, &other_focus).is_none());
    let taken = e.memory.take_offer_approval(&token, &owner).expect("taken");
    assert_eq!(taken.digest, p.digest);
    assert!(
        e.memory.take_offer_approval(&token, &owner).is_none(),
        "single use"
    );
    // And the spent token approves nothing more.
    assert_eq!(
        approve(&e, &request(&e, None, Some(&token), &p.digest))
            .await
            .unwrap_err(),
        "invalid_plan_token"
    );
}

#[tokio::test]
async fn a_chatless_token_is_checked_for_shape_owner_and_digest() {
    let e = env().await;
    let p = plan(&e, None).await.unwrap();
    let token = p.plan_token.clone().unwrap();
    for bad in [
        "",
        "short",
        &"G".repeat(32),
        &"a".repeat(31),
        &"a".repeat(33),
    ] {
        assert_eq!(
            approve(&e, &request(&e, None, Some(bad), &p.digest))
                .await
                .unwrap_err(),
            "invalid_plan_token",
            "{bad}"
        );
    }
    // A well-formed token nobody registered.
    assert_eq!(
        approve(&e, &request(&e, None, Some(&"a".repeat(32)), &p.digest))
            .await
            .unwrap_err(),
        "invalid_plan_token"
    );
    // A stale digest with a real token.
    assert_eq!(
        approve(&e, &request(&e, None, Some(&token), &"0".repeat(64)))
            .await
            .unwrap_err(),
        "plan_changed"
    );
    // Another user's session cannot approve this offer (the owner differs).
    let other = add_user(&e.pool, "Other User").await;
    let other_persona = uuid::Uuid::new_v4().to_string();
    persona_store::create_persona(
        &e.pool,
        &other_persona,
        "O",
        "personal",
        &other,
        &e.key,
        None,
    )
    .await
    .unwrap();
    settings_for(&e.pool, &other_persona, FOCUS).await;
    let other_env = PlanEnv {
        pool: &e.pool,
        scheduler: Arc::clone(&e.scheduler),
        memory: &e.memory,
        user_id: &other,
        key_hex: &e.key,
    };
    let mut r = request(&e, None, Some(&token), &p.digest);
    r.persona_id = other_persona;
    assert_eq!(
        approve_run_plan_for_session(&other_env, &r)
            .await
            .unwrap_err(),
        "invalid_plan_token"
    );
    // The owner still can.
    assert!(
        approve(&e, &request(&e, None, Some(&token), &p.digest))
            .await
            .unwrap()
            .approved
    );
}

#[tokio::test]
async fn revoking_a_chatless_offer_removes_it() {
    let e = env().await;
    let p = plan(&e, None).await.unwrap();
    let token = p.plan_token.unwrap();
    let r =
        revoke_run_plan_approval_for_session(&e.plan_env(), &e.persona, None, Some(&token), FOCUS)
            .await
            .unwrap();
    assert_eq!(
        r,
        RunPlanRevocationView {
            memory: true,
            persisted: false
        }
    );
    assert_eq!(e.memory.offer_count(), 0);
    let again =
        revoke_run_plan_approval_for_session(&e.plan_env(), &e.persona, None, Some(&token), FOCUS)
            .await
            .unwrap();
    assert!(!again.memory);
}

#[tokio::test]
async fn exactly_one_of_chat_and_token_names_the_target() {
    let e = env().await;
    let p = plan(&e, None).await.unwrap();
    let token = p.plan_token.clone().unwrap();
    let both = request(&e, Some(CHAT), Some(&token), &p.digest);
    let neither = request(&e, None, None, &p.digest);
    for r in [both, neither] {
        assert_eq!(approve(&e, &r).await.unwrap_err(), "invalid_context_key");
    }
    for (c, t) in [(Some(CHAT), Some(token.as_str())), (None, None)] {
        let r = revoke_run_plan_approval_for_session(&e.plan_env(), &e.persona, c, t, FOCUS)
            .await
            .unwrap_err();
        assert_eq!(r, "invalid_context_key");
    }
}

#[tokio::test]
async fn choices_must_be_offered_selectable_and_unique() {
    let e = env().await;
    let p = plan(&e, Some(CHAT)).await.unwrap();
    let good = unmeasured_pin(&p);
    let mut unknown_step = good.clone();
    unknown_step.action_id = "no_such_step".to_owned();
    let mut local = good.clone();
    local.strategy = "local".to_owned();
    let mut garbage = good.clone();
    garbage.strategy = "free_for_all".to_owned();
    let mut not_offered = good.clone();
    not_offered.model_id = "no-such-model".to_owned();
    let mut hosted = good.clone();
    hosted.strategy = "paid_api".to_owned();
    hosted.provider_id = "no-such-provider".to_owned();
    for (label, choices) in [
        ("unknown step", vec![unknown_step]),
        ("local is not a choice", vec![local]),
        ("unknown strategy word", vec![garbage]),
        ("not offered", vec![not_offered]),
        ("hosted not offered", vec![hosted]),
        ("duplicate", vec![good.clone(), good.clone()]),
        ("too many", vec![good.clone(); MAX_CHOICES + 1]),
    ] {
        let mut r = request(&e, Some(CHAT), None, &p.digest);
        r.choices = choices;
        assert_eq!(
            approve(&e, &r).await.unwrap_err(),
            "invalid_choice",
            "{label}"
        );
    }
    assert_eq!(rows(&e, "plan_approvals").await, 0);
    // The good one works.
    let mut r = request(&e, Some(CHAT), None, &p.digest);
    r.choices = vec![good];
    assert!(approve(&e, &r).await.unwrap().approved);
}

#[tokio::test]
async fn the_ceiling_is_decimal_digits_within_the_bound() {
    let e = env().await;
    let p = plan(&e, Some(CHAT)).await.unwrap();
    for bad in [
        "",
        "-1",
        "+5",
        "1.5",
        " 7",
        "7 ",
        "1e3",
        "abc",
        "99999999999999999999999",
        "25000001",
    ] {
        let mut r = request(&e, Some(CHAT), None, &p.digest);
        r.ceiling_micros = bad.to_owned();
        assert_eq!(
            approve(&e, &r).await.unwrap_err(),
            "invalid_ceiling",
            "{bad:?}"
        );
    }
    for good in ["0", "1", "25000000"] {
        let mut r = request(&e, Some(CHAT), None, &p.digest);
        r.ceiling_micros = good.to_owned();
        assert_eq!(
            approve(&e, &r).await.unwrap().ceiling_micros.as_deref(),
            Some(good)
        );
    }
}

#[test]
fn every_approval_error_has_one_short_code() {
    use ApprovalError::*;
    let table = [
        (PlanChanged, "plan_changed"),
        (UnknownStep, "invalid_choice"),
        (DuplicatePin, "invalid_choice"),
        (PinNotAllowed, "invalid_choice"),
        (PinNotOffered, "invalid_choice"),
        (NotSelectable, "invalid_choice"),
        (CeilingBelowEstimate, "invalid_ceiling"),
        (CeilingTooHigh, "invalid_ceiling"),
        (BadChoices, "plan_unavailable"),
        (Unreadable, "plan_unavailable"),
        (Unavailable, "plan_unavailable"),
    ];
    for (err, code) in table {
        assert_eq!(approval_error(err), code);
    }
}

// -- the other error codes -----------------------------------------------------------------

#[tokio::test]
async fn persona_focus_context_and_run_ids_are_validated_alike_on_every_command() {
    let e = env().await;
    let pe = e.plan_env();
    let other_user = add_user(&e.pool, "Stranger").await;
    let foreign = uuid::Uuid::new_v4().to_string();
    persona_store::create_persona(
        &e.pool,
        &foreign,
        "F",
        "personal",
        &other_user,
        &e.key,
        None,
    )
    .await
    .unwrap();
    // Malformed, non-canonical and foreign Persona ids answer alike.
    let upper = e.persona.to_uppercase();
    for bad in [
        "",
        "not-a-uuid",
        &upper,
        &foreign,
        "00000000-0000-0000-0000-000000000000",
    ] {
        let a = get_run_plan_for_session(&pe, bad, Some(CHAT), FOCUS)
            .await
            .unwrap_err();
        let b = get_run_plan_approval_for_session(&pe, bad, CHAT, FOCUS)
            .await
            .unwrap_err();
        let c = revoke_run_plan_approval_for_session(&pe, bad, Some(CHAT), None, FOCUS)
            .await
            .unwrap_err();
        let d = get_run_park_for_session(&pe, bad, "run-1")
            .await
            .unwrap_err();
        let mut r = request(&e, Some(CHAT), None, "x");
        r.persona_id = bad.to_owned();
        let f = approve_run_plan_for_session(&pe, &r).await.unwrap_err();
        for got in [a, b, c, d, f] {
            assert_eq!(got, "persona_not_found", "{bad}");
        }
    }
    // Focus ids: a slug, and one that exists.
    for bad in ["", "Has Caps", "has space", "a/b", &"x".repeat(65)] {
        let a = get_run_plan_for_session(&pe, &e.persona, Some(CHAT), bad)
            .await
            .unwrap_err();
        let b = get_run_plan_approval_for_session(&pe, &e.persona, CHAT, bad)
            .await
            .unwrap_err();
        assert_eq!(
            (a.as_str(), b.as_str()),
            ("invalid_focus_id", "invalid_focus_id"),
            "{bad}"
        );
    }
    assert_eq!(
        get_run_plan_for_session(&pe, &e.persona, Some(CHAT), "no-such-focus")
            .await
            .unwrap_err(),
        "invalid_focus_id"
    );
    // Context keys.
    let long_key = "c".repeat(10_000);
    for bad in ["", "nul\0key", long_key.as_str()] {
        let a = get_run_plan_for_session(&pe, &e.persona, Some(bad), FOCUS)
            .await
            .unwrap_err();
        let b = get_run_plan_approval_for_session(&pe, &e.persona, bad, FOCUS)
            .await
            .unwrap_err();
        assert_eq!(
            (a.as_str(), b.as_str()),
            ("invalid_context_key", "invalid_context_key"),
            "{bad:?}"
        );
    }
    // Run ids.
    for bad in ["", "has space", "a/b", "x;drop", &"r".repeat(129)] {
        assert_eq!(
            get_run_park_for_session(&pe, &e.persona, bad)
                .await
                .unwrap_err(),
            "invalid_run_id",
            "{bad:?}"
        );
    }
}

#[tokio::test]
async fn a_focus_that_cannot_be_authorized_is_plan_unavailable_with_no_detail() {
    let e = env().await;
    // No focus_settings row and no composition: the run cannot start, so no plan is offered.
    let err = get_run_plan_for_session(&e.plan_env(), &e.persona, Some(CHAT), NO_SETTINGS)
        .await
        .unwrap_err();
    assert_eq!(err, "plan_unavailable");
    assert!(ERROR_CODES.contains(&err.as_str()));
}

#[test]
fn the_path_cap_and_other_plan_failures_have_their_own_codes() {
    // PlanOfferError::TooManyPaths is what run_plan::build's cap becomes (lifecycle's
    // plan_offer); the command answers it with its own code and builds no offer.
    let src = include_str!("run_plan.rs");
    let code = src.split("#[cfg(test)]").next().unwrap();
    assert!(code
        .contains("Err(PlanOfferError::TooManyPaths) => Err(\"plan_too_many_paths\".to_owned())"));
    assert!(code.contains("Err(PlanOfferError::Unavailable) => Err(unavailable("));
}

// -- get_run_park --------------------------------------------------------------------------

fn park(tag: &str) -> ParkRecord {
    ParkRecord {
        code: Code::ResolverTooWeak,
        action_id: "draft".to_owned(),
        params_json: format!("{{\"tag\":\"{tag}\"}}"),
    }
}

#[tokio::test]
async fn a_park_is_read_from_memory_first_then_from_the_table() {
    let e = env().await;
    let pe = e.plan_env();
    assert_eq!(
        get_run_park_for_session(&pe, &e.persona, "run-1")
            .await
            .unwrap(),
        None
    );

    // A persisted park.
    let mut c = conn(&e).await;
    sqlx::query("INSERT INTO focus_runs (id, focus_id, status, is_fast_lane, is_quick_ask, started_at, notes) VALUES ('run-1', 'f', 'awaiting_user', 0, 0, 't', '{}')")
        .execute(&mut c)
        .await
        .unwrap();
    run_park_store::put_park(
        &mut c,
        &run_park_store::ParkWrite {
            run_id: "run-1",
            action_id: "draft",
            code: "resolver.no_candidate",
            params_json: "{\"reason\":\"none_ready\"}",
        },
    )
    .await
    .unwrap();
    drop(c);
    let v = get_run_park_for_session(&pe, &e.persona, "run-1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(v.run_id, "run-1");
    assert_eq!(v.action_id, "draft");
    assert_eq!(v.code, "resolver.no_candidate");
    assert_eq!(v.params_json, "{\"reason\":\"none_ready\"}");

    // A held park wins, and is scoped to the user and Persona.
    e.memory
        .put_run_park(&e.user, &e.persona, "run-1", park("held"));
    let v = get_run_park_for_session(&pe, &e.persona, "run-1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(v.code, "resolver.too_weak");
    assert_eq!(v.params_json, "{\"tag\":\"held\"}");
    e.memory
        .put_run_park("another-user", &e.persona, "run-2", park("x"));
    assert_eq!(
        get_run_park_for_session(&pe, &e.persona, "run-2")
            .await
            .unwrap(),
        None
    );
    // Reads write nothing.
    assert_eq!(rows(&e, "run_parks").await, 1);
}

// -- text, order and shape of the module --------------------------------------------------

fn code_of_module() -> String {
    include_str!("run_plan.rs")
        .split("#[cfg(test)]")
        .next()
        .unwrap()
        .lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn walk_strings(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::String(s) => out.push(s.clone()),
        Value::Array(a) => a.iter().for_each(|x| walk_strings(x, out)),
        Value::Object(o) => o.values().for_each(|x| walk_strings(x, out)),
        _ => {}
    }
}

/// No DTO field can carry text: every string in a real, fully populated plan, approval and
/// park view is an id, a closed word or a decimal number (a shape no prose has).
#[tokio::test]
async fn every_string_a_view_carries_is_an_id_a_closed_word_or_a_number() {
    let e = env().await;
    let p = plan(&e, None).await.unwrap();
    let pin = unmeasured_pin(&p);
    let token = p.plan_token.clone().unwrap();
    let mut r = request(&e, None, Some(&token), &p.digest);
    r.choices = vec![pin];
    r.ceiling_micros = "12345".to_owned();
    let approved = approve(&e, &r).await.unwrap();
    e.memory
        .put_run_park(&e.user, &e.persona, "run-1", park("t"));
    let parked = get_run_park_for_session(&e.plan_env(), &e.persona, "run-1")
        .await
        .unwrap();

    let mut strings = Vec::new();
    walk_strings(&serde_json::to_value(&p).unwrap(), &mut strings);
    walk_strings(&serde_json::to_value(&approved).unwrap(), &mut strings);
    let mut parked_view = serde_json::to_value(&parked).unwrap();
    // params_json is itself a JSON object of the filtered shape: check its parsed strings.
    if let Some(inner) = parked_view.get("params_json").and_then(Value::as_str) {
        let parsed: Value = serde_json::from_str(inner).unwrap();
        parked_view["params_json"] = parsed;
    }
    walk_strings(&parked_view, &mut strings);
    assert!(strings.len() > 30, "a populated view: {}", strings.len());
    for s in &strings {
        assert!(s.len() <= 128, "{s}");
        assert!(
            s.chars().all(
                |c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | ':' | '/' | '+' | '-')
            ),
            "not an id, a closed word or a number: {s:?}"
        );
    }
}

#[test]
fn the_module_names_no_text_source_and_logs_no_value() {
    let code = code_of_module();
    for banned in [
        "display_name",
        "prompt",
        "user_input",
        "persona_context",
        "voice_profile",
        "content",
        "template",
        "{e}",
        "{e:?}",
        "format!(\"{",
        "println!",
        "eprintln!",
        "dbg!",
    ] {
        assert!(!code.contains(banned), "run_plan.rs names {banned}");
    }
    // The one log call logs a fixed string.
    assert_eq!(code.matches("log::").count(), 1);
    assert!(code.contains("log::warn!(\"run_plan: {what}\")"));
    assert!(!code.contains("output_store::open_outputs_db("));
    assert!(code.contains("output_store::open_outputs_db_retrying("));
}

/// Every command takes the writer gate first, derives the user and key from the session, and
/// has no user_id argument to spoof.
#[test]
fn every_command_takes_the_writer_gate_first_and_the_user_from_the_session() {
    let code = code_of_module();
    for name in [
        "get_run_plan",
        "approve_run_plan",
        "get_run_plan_approval",
        "revoke_run_plan_approval",
        "get_run_park",
    ] {
        let head = format!("pub async fn {name}(");
        let body = code.split(&head).nth(1).unwrap_or_else(|| panic!("{name}"));
        let sig = body.split(") ->").next().unwrap();
        assert!(!sig.contains("user_id"), "{name} takes a user id");
        let body = body.split("\n}\n").next().unwrap();
        let gate = body.find("writer_gate::command(").expect("gate");
        let session = body.find("session_user_and_key(").expect("session");
        assert!(gate < session, "{name}: the gate comes first");
        assert!(body.contains("_for_session("), "{name}");
    }
    // Reads are the commands that never name a writer.
    for read in [
        "get_run_plan_approval_for_session",
        "get_run_park_for_session",
    ] {
        let body = code.split(&format!("async fn {read}(")).nth(1).unwrap();
        let body = body.split("\n}\n").next().unwrap();
        for writer in [
            "save_chat_approval",
            "revoke_chat_approval",
            "approve_offer",
            "register_offer",
            "put_park",
            "clear_park",
        ] {
            assert!(!body.contains(writer), "{read} names {writer}");
        }
    }
}

#[test]
fn every_error_code_has_a_placeholder_key_in_en_json() {
    let json: Value =
        serde_json::from_str(include_str!("../../../frontend/src/i18n/en.json")).unwrap();
    for code in ERROR_CODES {
        let text = json["plan"]["error"][code]
            .as_str()
            .unwrap_or_else(|| panic!("en.json has no plan.error.{code}"));
        assert!(!text.trim().is_empty(), "{code}");
    }
    let keys: Vec<&String> = json["plan"]["error"].as_object().unwrap().keys().collect();
    assert_eq!(keys.len(), ERROR_CODES.len(), "{keys:?}");
    // Every code the module returns directly is in the list, and every code is used.
    let code = code_of_module();
    for (i, _) in code.match_indices("Err(\"") {
        let lit = code[i + 5..].split('"').next().unwrap();
        assert!(ERROR_CODES.contains(&lit), "{lit}");
    }
    for c in ERROR_CODES {
        assert!(code.contains(&format!("\"{c}\"")), "{c} is never returned");
    }
}

#[test]
fn the_commands_are_registered_in_the_ipc_surface() {
    let ipc = include_str!("../ipc.rs");
    for name in [
        "get_run_plan",
        "approve_run_plan",
        "get_run_plan_approval",
        "revoke_run_plan_approval",
        "get_run_park",
    ] {
        assert!(
            ipc.contains(&format!("commands::run_plan::{name},")),
            "{name}"
        );
    }
}
