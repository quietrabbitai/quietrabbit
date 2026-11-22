// src-tauri/src/conductor/plan_gate_tests.rs
//
// Tests of the plan gate (items.id=901 stage 4): where it runs, what it reads, what it
// returns, and what it never does. The executor's envelope check has its own tests beside the
// resolver's in executor.rs.

use std::collections::HashSet;

use super::*;
use crate::conductor::concurrency::ConductorScheduler;
use crate::conductor::guard::Guard;
use crate::conductor::model_block::config::{Destination, QualityBar};
use crate::conductor::plan_approval::{approve, save_chat_approval, Home};
use crate::conductor::plan_memory::PlanMemory;
use crate::conductor::run_plan::tests::{
    def_of, fixture_key_hex, hosted_req, hosted_rig, launch, local_req, planned, requirement, step,
    with_temp_shared_db, HOST, MODEL, SPEND_USER,
};
use crate::conductor::run_plan::{OfferStrategy, Pin};
use crate::conductor::tokens::ExternalAccess;
use crate::persistence::session_store::latch_write_suppressed;

const PERSONA: &str = "persona-1";
const CHAT: &str = "chat-1";
const FOCUS: &str = "fixture-focus";

fn run_for(
    pool: sqlx::SqlitePool,
    steps: Vec<crate::conductor::tokens::ActionDefinition>,
    key_hex: &str,
    memory: Option<PlanMemory>,
) -> FocusRun {
    let mut run: FocusRun = FocusRun::new(
        SPEND_USER.to_owned(),
        PERSONA.to_owned(),
        FOCUS.to_owned(),
        pool,
        Arc::new(ConductorScheduler::new()),
        String::new(),
        false,
        Some(key_hex.to_owned()),
        None,
        false,
        HashSet::new(),
        None,
    );
    run.focus_def = Some(def_of(steps));
    run.launch = launch(None);
    run._focus_external_access = ExternalAccess::AnonymousPreferred;
    run.context_key = Some(CHAT.to_owned());
    run.plan_memory = memory;
    run
}

fn dead_pool() -> sqlx::SqlitePool {
    sqlx::SqlitePool::connect_lazy("sqlite:///qr-901-no-such-dir/none.db").unwrap()
}

fn scope() -> ChatScope {
    ChatScope {
        user_id: SPEND_USER.to_owned(),
        persona_id: PERSONA.to_owned(),
        context_key: CHAT.to_owned(),
        focus_id: FOCUS.to_owned(),
    }
}

fn hosted_steps() -> Vec<crate::conductor::tokens::ActionDefinition> {
    vec![planned("draft", hosted_req())]
}

async fn outputs_conn(key_hex: &str) -> sqlx::SqliteConnection {
    crate::persistence::output_store::open_outputs_db_retrying(SPEND_USER, PERSONA, key_hex)
        .await
        .expect("outputs.db opens under the test root")
}

/// The gate's first answer to a run that has no approval, and the plan it offered.
async fn required_plan(run: &mut FocusRun) -> RunPlan {
    match run.plan_gate().await.expect("the plan can be built") {
        PlanGate::Required(plan) => *plan,
        PlanGate::Proceed => panic!("expected the plan to need an approval"),
    }
}

// ---------------------------------------------------------------------------
// What the gate skips
// ---------------------------------------------------------------------------

/// A Focus with no model block: no plan, and no read of any kind (a pool that can never
/// connect, no key, no plan memory).
#[tokio::test]
async fn a_focus_with_no_model_block_skips_the_gate_with_no_read_or_write() {
    let pool = dead_pool();
    let mut run = run_for(pool.clone(), vec![step("a"), step("b")], "", None);
    assert!(matches!(run.plan_gate().await, Ok(PlanGate::Proceed)));
    assert!(run.plan.is_none());
    assert_eq!(pool.size(), 0, "no connection was attempted");
    assert!(run.focus_run_id.is_none(), "and no run row was opened");
}

/// A bound API Chat run keeps its bound destination and never resolves.
#[tokio::test]
async fn a_bound_api_chat_run_skips_the_gate() {
    let pool = dead_pool();
    let mut run = run_for(pool.clone(), hosted_steps(), "", None);
    run.chat_binding = Some(crate::api_chat::BoundDestination {
        provider_id: "host".to_owned(),
        model_id: "m".to_owned(),
    });
    assert!(matches!(run.plan_gate().await, Ok(PlanGate::Proceed)));
    assert_eq!(pool.size(), 0);
}

/// More paths than the cap: the run does not start, whatever it asks for.
#[tokio::test]
async fn a_plan_that_cannot_be_built_stops_the_start_closed() {
    let mut steps = Vec::new();
    for i in 0..9 {
        let var = format!("r{i}");
        let mut router = step(&format!("router{i}"));
        router.output_var = Some(var.clone());
        steps.push(router);
        for (name, op) in [("yes", "eq"), ("no", "ne")] {
            let mut s = step(&format!("{name}{i}"));
            s.guard = Guard::from_columns(Some(&var), Some(op), Some("a")).unwrap();
            steps.push(s);
        }
    }
    steps.push(planned("draft", hosted_req()));
    let mut run = run_for(dead_pool(), steps, "", Some(PlanMemory::default()));
    assert!(matches!(
        run.plan_gate().await,
        Err(LifecycleError::PlanUnavailable)
    ));
    assert_eq!(
        LifecycleError::PlanUnavailable.to_string(),
        "plan_unavailable"
    );
}

// ---------------------------------------------------------------------------
// The approval, by chat
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_plan_that_may_go_hosted_needs_an_approval_and_a_verified_one_lets_the_run_start() {
    with_temp_shared_db(|pool| async move {
        let key_hex = fixture_key_hex();
        hosted_rig(&pool, Some(crate::conductor::run_plan::tests::PRICE_TEXT)).await;
        let memory = PlanMemory::default();
        let mut run = run_for(pool.clone(), hosted_steps(), &key_hex, Some(memory.clone()));

        // No approval: the plan is offered, nothing is written, no run row is opened.
        let plan = required_plan(&mut run).await;
        assert!(plan.needs_approval);
        assert!(run.plan.is_none());
        assert!(run.focus_run_id.is_none());

        // The user approves what they were shown (persisted: an unlatched chat).
        let paid = Pin {
            action_id: "draft".to_owned(),
            strategy: OfferStrategy::PaidApi,
            provider_id: HOST.to_owned(),
            model_id: MODEL.to_owned(),
        };
        let a = approve(&plan, &plan.digest, 25_000, vec![paid.clone()]).unwrap();
        let mut conn = outputs_conn(&key_hex).await;
        let home = save_chat_approval(&memory, Some(&mut conn), &scope(), &a)
            .await
            .unwrap();
        assert!(matches!(home, Home::Persisted { .. }));

        // Calling again, the gate reads it, verifies it against the plan it just built and
        // keeps it on the run for the executor.
        let mut again = run_for(pool.clone(), hosted_steps(), &key_hex, Some(memory.clone()));
        assert!(matches!(again.plan_gate().await, Ok(PlanGate::Proceed)));
        let held = again.plan.as_ref().expect("the approval is on the run");
        assert_eq!(held.pins, vec![paid]);
        assert!(
            again.focus_run_id.is_none(),
            "the gate never opens the record"
        );

        // A plan that is no longer the one that was approved needs a fresh approval.
        let mut changed = run_for(
            pool.clone(),
            vec![planned(
                "draft",
                requirement(
                    QualityBar::Standard,
                    QualityBar::High,
                    &[Destination::Local, Destination::HostedApi],
                    ExternalAccess::AnonymousPreferred,
                ),
            )],
            &key_hex,
            Some(memory.clone()),
        );
        assert!(matches!(
            changed.plan_gate().await,
            Ok(PlanGate::Required(_))
        ));
        assert!(changed.plan.is_none());
    })
    .await;
}

/// A latched chat keeps its approval in memory and the gate reads it from there.
#[tokio::test]
async fn an_approval_in_the_memory_home_of_a_latched_chat_is_read_by_the_gate() {
    with_temp_shared_db(|pool| async move {
        let key_hex = fixture_key_hex();
        hosted_rig(&pool, Some(crate::conductor::run_plan::tests::PRICE_TEXT)).await;
        let memory = PlanMemory::default();
        let mut run = run_for(pool.clone(), hosted_steps(), &key_hex, Some(memory.clone()));
        let plan = required_plan(&mut run).await;

        let mut conn = outputs_conn(&key_hex).await;
        latch_write_suppressed(&mut conn, CHAT, FOCUS, "run-a")
            .await
            .unwrap();
        let a = approve(&plan, &plan.digest, 0, vec![]).unwrap();
        assert_eq!(
            save_chat_approval(&memory, Some(&mut conn), &scope(), &a)
                .await
                .unwrap(),
            Home::Memory
        );
        let mut again = run_for(pool.clone(), hosted_steps(), &key_hex, Some(memory));
        assert!(matches!(again.plan_gate().await, Ok(PlanGate::Proceed)));
        assert!(again.plan.is_some());
    })
    .await;
}

/// items.id=888 stage 4: a resumed run of a chat is given an EMPTY memory home when there is
/// no app handle (rehydrate_focus_run), so it reads the PERSISTED approval of its chat and
/// verifies it against the plan it builds, as a fresh run does.
#[tokio::test]
async fn an_empty_memory_home_still_reads_the_persisted_approval() {
    with_temp_shared_db(|pool| async move {
        let key_hex = fixture_key_hex();
        hosted_rig(&pool, Some(crate::conductor::run_plan::tests::PRICE_TEXT)).await;
        let memory = PlanMemory::default();
        let mut run = run_for(pool.clone(), hosted_steps(), &key_hex, Some(memory.clone()));
        let plan = required_plan(&mut run).await;
        let a = approve(&plan, &plan.digest, 0, vec![]).unwrap();
        let mut conn = outputs_conn(&key_hex).await;
        assert!(matches!(
            save_chat_approval(&memory, Some(&mut conn), &scope(), &a)
                .await
                .unwrap(),
            Home::Persisted { .. }
        ));
        let mut resumed = run_for(
            pool.clone(),
            hosted_steps(),
            &key_hex,
            Some(PlanMemory::default()),
        );
        resumed.rehydrated = true;
        assert!(matches!(resumed.plan_gate().await, Ok(PlanGate::Proceed)));
        assert!(
            resumed.plan.is_some(),
            "the persisted approval was read and verified"
        );
    })
    .await;
}

/// No plan memory (a rehydrated or swept run) reads no approval: it asks again, it never
/// proceeds on one it could not read. An approval row that cannot be read is the same.
#[tokio::test]
async fn an_approval_that_cannot_be_read_is_never_permission() {
    with_temp_shared_db(|pool| async move {
        let key_hex = fixture_key_hex();
        hosted_rig(&pool, Some(crate::conductor::run_plan::tests::PRICE_TEXT)).await;
        let memory = PlanMemory::default();
        let mut run = run_for(pool.clone(), hosted_steps(), &key_hex, Some(memory.clone()));
        let plan = required_plan(&mut run).await;
        let a = approve(&plan, &plan.digest, 0, vec![]).unwrap();
        let mut conn = outputs_conn(&key_hex).await;
        save_chat_approval(&memory, Some(&mut conn), &scope(), &a)
            .await
            .unwrap();

        // The approval exists, but this run has no plan memory.
        let mut no_memory = run_for(pool.clone(), hosted_steps(), &key_hex, None);
        assert!(matches!(
            no_memory.plan_gate().await,
            Ok(PlanGate::Required(_))
        ));

        // The persisted row cannot be read.
        sqlx::query("DROP TABLE plan_approvals")
            .execute(&mut conn)
            .await
            .unwrap();
        let mut unreadable = run_for(pool.clone(), hosted_steps(), &key_hex, Some(memory));
        assert!(matches!(
            unreadable.plan_gate().await,
            Ok(PlanGate::Required(_))
        ));
        assert!(unreadable.plan.is_none());
    })
    .await;
}

/// A plan that cannot go hosted needs no approval; an approval with a pin for an
/// unmeasured local model is still kept on the run so the executor can use the pin.
#[tokio::test]
async fn a_local_only_plan_needs_no_approval_and_keeps_a_pin_it_was_given() {
    with_temp_shared_db(|pool| async move {
        let key_hex = fixture_key_hex();
        hosted_rig(&pool, None).await;
        let memory = PlanMemory::default();
        let steps = vec![planned("draft", local_req())];

        let mut run = run_for(pool.clone(), steps.clone(), &key_hex, Some(memory.clone()));
        assert!(matches!(run.plan_gate().await, Ok(PlanGate::Proceed)));
        assert!(run.plan.is_none(), "no approval, only resolver local picks");

        // The plan offers the unmeasured local models (the shipped data has no task-wide
        // cell), and the user pins one.
        let plan = crate::conductor::run_plan::build(&BuildInput {
            pool: &pool,
            user_id: SPEND_USER,
            key_hex: &key_hex,
            def: &def_of(steps.clone()),
            launch: &launch(None),
            focus_ceiling: ExternalAccess::AnonymousPreferred,
        })
        .await
        .unwrap()
        .unwrap();
        assert!(!plan.needs_approval);
        let offer = plan.steps[0]
            .offers
            .iter()
            .find(|o| o.strategy == OfferStrategy::LocalUnmeasured && o.selectable)
            .expect("an unmeasured local model is offered")
            .clone();
        let pin = Pin {
            action_id: "draft".to_owned(),
            strategy: OfferStrategy::LocalUnmeasured,
            provider_id: String::new(),
            model_id: offer.model_id,
        };
        let a = approve(&plan, &plan.digest, 0, vec![pin.clone()]).unwrap();
        let mut conn = outputs_conn(&key_hex).await;
        save_chat_approval(&memory, Some(&mut conn), &scope(), &a)
            .await
            .unwrap();
        let mut pinned = run_for(pool.clone(), steps, &key_hex, Some(memory));
        assert!(matches!(pinned.plan_gate().await, Ok(PlanGate::Proceed)));
        assert_eq!(pinned.plan.as_ref().unwrap().pins, vec![pin]);
    })
    .await;
}

// ---------------------------------------------------------------------------
// A run with no chat: the token
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_chatless_run_is_approved_by_its_token_which_the_run_consumes_even_when_it_fails() {
    with_temp_shared_db(|pool| async move {
        let key_hex = fixture_key_hex();
        hosted_rig(&pool, Some(crate::conductor::run_plan::tests::PRICE_TEXT)).await;
        let memory = PlanMemory::default();
        let owner = OfferOwner {
            user_id: SPEND_USER.to_owned(),
            persona_id: PERSONA.to_owned(),
            focus_id: FOCUS.to_owned(),
        };
        let chatless = |token: Option<&str>| {
            let mut run = run_for(pool.clone(), hosted_steps(), &key_hex, Some(memory.clone()));
            run.context_key = None;
            run.plan_token = token.map(str::to_owned);
            run
        };

        // No token: the plan is offered.
        let plan = required_plan(&mut chatless(None)).await;

        // get_run_plan registered the offer, approve_run_plan attached the approval.
        let token = memory.register_offer(&owner, &plan.digest).unwrap();
        let a = approve(&plan, &plan.digest, 0, vec![]).unwrap();
        memory
            .approve_offer(&token, &owner, &plan.digest, a.clone())
            .unwrap();
        let mut run = chatless(Some(&token));
        assert!(matches!(run.plan_gate().await, Ok(PlanGate::Proceed)));
        assert!(run.plan.is_some());
        // The run consumed the token: a later run cannot reuse it.
        let mut later = chatless(Some(&token));
        assert!(matches!(later.plan_gate().await, Ok(PlanGate::Required(_))));

        // A token whose approval no longer verifies is STILL consumed: the run it was
        // presented to spent it, and the user is offered the plan again.
        let stale = memory.register_offer(&owner, &plan.digest).unwrap();
        let mut forged = a;
        forged.digest = "0".repeat(64);
        memory
            .approve_offer(&stale, &owner, &plan.digest, forged)
            .unwrap();
        let mut failing = chatless(Some(&stale));
        assert!(matches!(
            failing.plan_gate().await,
            Ok(PlanGate::Required(_))
        ));
        assert!(
            memory.take_offer_approval(&stale, &owner).is_none(),
            "consumed even though verification failed"
        );
        // Another user's run cannot present the token at all.
        let foreign = memory.register_offer(&owner, &plan.digest).unwrap();
        memory
            .approve_offer(
                &foreign,
                &owner,
                &plan.digest,
                approve(&plan, &plan.digest, 0, vec![]).unwrap(),
            )
            .unwrap();
        let mut other = chatless(Some(&foreign));
        other.user_id = "someone-else".to_owned();
        assert!(matches!(other.plan_gate().await, Ok(PlanGate::Required(_))));
        assert!(
            memory.take_offer_approval(&foreign, &owner).is_some(),
            "and the owner's offer is untouched"
        );
    })
    .await;
}

// ---------------------------------------------------------------------------
// The preflight's cheap read
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_focus_has_a_model_block_only_when_its_composition_cites_one() {
    with_temp_shared_db(|pool| async move {
        // items.id=903: the Writing Assistant is built on model blocks.
        assert!(focus_has_model_block(&pool, "writing-assistant")
            .await
            .unwrap());
        for id in [
            "quick-ask",
            "research-and-buy",
            "role-assessment",
            "qr-admin",
            "api-chat",
            "no-such-focus",
        ] {
            assert!(
                !focus_has_model_block(&pool, id).await.unwrap(),
                "{id} has no model block"
            );
        }
        // A composition that cites a model block.
        sqlx::query(
            "INSERT INTO focuses
               (focus_id, display_name, max_routing_tier, status, created_at, updated_at)
             VALUES ('model-focus', 'Model Focus', 'local_only', 'designed', 't', 't')",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO focus_block_compositions
               (id, focus_id, action_id, block_stable_id, sequence_index, created_at, updated_at)
             VALUES ('c1', 'model-focus', 'draft', 'cb-12', 0, 't', 't')",
        )
        .execute(&pool)
        .await
        .unwrap();
        assert!(focus_has_model_block(&pool, "model-focus").await.unwrap());
        assert!(!focus_has_model_block(&pool, "quick-ask").await.unwrap());
    })
    .await;
}

// ---------------------------------------------------------------------------
// Pins on the structure of the start (source scans)
// ---------------------------------------------------------------------------

/// The production part of a source file: everything before its test module (a bare
/// `#[cfg(test)]` on a field is not the end of the code).
fn code_of(src: &str) -> String {
    let end = ["\n#[cfg(test)]\nmod ", "\n#[cfg(test)]\n#[path"]
        .iter()
        .filter_map(|m| src.find(m))
        .min()
        .unwrap_or(src.len());
    src[..end].to_owned()
}

/// Whether `needle` occurs as a whole path: not followed by an identifier character.
fn has_path(code: &str, needle: &str) -> bool {
    code.match_indices(needle).any(|(i, _)| {
        !code[i + needle.len()..]
            .chars()
            .next()
            .is_some_and(|c| c.is_alphanumeric() || c == '_')
    })
}

fn squash(s: &str) -> String {
    s.chars().filter(|c| !c.is_whitespace()).collect()
}

/// AUTHORIZE is exactly its two halves, in order, and nothing else: existing callers of
/// authorize() behave as before, and the record is written only by open_record().
#[test]
fn authorize_is_exactly_the_checks_then_the_record() {
    let src = code_of(include_str!("lifecycle.rs"));
    let body = src
        .split("pub async fn authorize(&mut self)")
        .nth(1)
        .unwrap()
        .split("\n    }\n")
        .next()
        .unwrap();
    assert_eq!(
        squash(body),
        squash("-> Result<(), LifecycleError> { self.authorize_checks().await?; self.open_record().await")
    );
    // The checks half writes nothing and the record half is the only writer.
    let checks = src
        .split("pub(crate) async fn authorize_checks(&mut self)")
        .nth(1)
        .unwrap()
        .split("\n    }\n")
        .next()
        .unwrap();
    assert!(!checks.contains("write_focus_run_record"));
    assert!(!checks.contains("focus_run_id"));
    let record = src
        .split("pub(crate) async fn open_record(&mut self)")
        .nth(1)
        .unwrap()
        .split("\n    }\n")
        .next()
        .unwrap();
    assert!(record.contains("write_focus_run_record(\"initializing\")"));
    // Nothing else in the start path writes the initializing row.
    assert_eq!(
        src.matches("write_focus_run_record(\"initializing\")")
            .count(),
        1
    );
}

/// The plan gate runs after the authorization checks and before the record is written.
#[test]
fn the_plan_gate_runs_between_the_checks_and_the_record() {
    let src = code_of(include_str!("../commands/execution.rs"));
    let body = src
        .split("pub(crate) async fn load_and_authorize_run")
        .nth(1)
        .unwrap()
        .split("pub(crate) async fn plan_preflight")
        .next()
        .unwrap();
    let at = |needle: &str| body.find(needle).unwrap_or_else(|| panic!("{needle}"));
    let load = at("run.load()");
    let checks = at("run.authorize_checks()");
    let gate = at("run.plan_gate()");
    let record = at("run.open_record()");
    assert!(load < checks && checks < gate && gate < record);
    assert!(
        !body.contains("run.authorize()"),
        "the halves are called separately"
    );
    assert!(!body.contains("write_focus_run_record"));
    // The preflight runs the same gate over a throwaway run and never opens the record.
    let pre = src
        .split("pub(crate) async fn plan_preflight")
        .nth(1)
        .unwrap()
        .split("#[tauri::command]")
        .next()
        .unwrap();
    assert!(pre.contains("run.plan_gate()"));
    assert!(!pre.contains("open_record"));
    assert!(!pre.contains("attach_lease"));
}

/// The stable string, and the three callers' handling of the typed outcome (API Chat's
/// outcome type is unchanged).
#[test]
fn plan_required_is_a_stable_string_and_every_caller_maps_the_typed_outcome() {
    assert_eq!(
        crate::commands::execution::PLAN_REQUIRED_CODE,
        "plan_required"
    );
    assert_eq!(
        crate::commands::execution::plan_required_error(),
        "plan_required"
    );
    for (name, src) in [
        ("execution.rs", include_str!("../commands/execution.rs")),
        ("messages.rs", include_str!("../commands/messages.rs")),
        ("api_chat.rs", include_str!("../commands/api_chat.rs")),
    ] {
        let code = code_of(src);
        assert!(code.contains("LoadOutcome::Ready(run) => *run"), "{name}");
        // send_message leaves the duplicate-turn marker first (stage 6), so its arm is a
        // block that still ends in the same refusal.
        assert!(
            code.contains("LoadOutcome::PlanRequired(_) => return Err(")
                || (code.contains("LoadOutcome::PlanRequired(_) => {")
                    && code.contains("return Err(execution::plan_required_error());")),
            "{name}"
        );
    }
    // SendApiChatOutcome is unchanged: no plan variant.
    let api = code_of(include_str!("../commands/api_chat.rs"));
    let outcome = api
        .split("pub enum SendApiChatOutcome")
        .nth(1)
        .unwrap()
        .split("\n}\n")
        .next()
        .unwrap();
    assert!(outcome.contains("Sent {") && outcome.contains("ReviewRequired {"));
    assert!(!outcome.to_lowercase().contains("plan"));
    // send_message's preflight comes before it saves the user's turn.
    let msgs = code_of(include_str!("../commands/messages.rs"));
    let send = msgs.split("pub async fn send_message").nth(1).unwrap();
    assert!(
        send.find("execution::plan_preflight(").unwrap()
            < send.find("turn_marker::save_user_turn_once(").unwrap()
    );
    // The request carries the optional token.
    assert!(code_of(include_str!("../commands/execution.rs"))
        .contains("pub plan_token: Option<String>"));
}

/// Every reader of the plan, the approval and the memory home outside their own modules, so
/// the allow-list can be checked line by line. A new reader fails this test and needs review.
#[test]
fn the_plan_the_approval_and_the_memory_home_are_read_only_where_reviewed() {
    fn walk(dir: std::path::PathBuf, f: &mut dyn FnMut(&str, &str)) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(path, f);
            } else if path.extension().is_some_and(|e| e == "rs") {
                f(
                    &path.to_string_lossy(),
                    &std::fs::read_to_string(&path).unwrap(),
                );
            }
        }
    }
    let skip = |name: &str| {
        name.ends_with("_tests.rs")
            || name.ends_with("conductor/run_plan.rs")
            || name.ends_with("conductor/plan_approval.rs")
            || name.ends_with("conductor/plan_memory.rs")
            || name.ends_with("persistence/plan_approval_store.rs")
            || name.ends_with("persistence/run_park_store.rs")
    };
    // (needle, allowed files). Production code only (before the first #[cfg(test)]).
    let rules: [(&str, &[&str]); 13] = [
        // The run's own approval: set by the gate, read for the StepContext and the Gate 3
        // look-ahead (lifecycle.rs) and nowhere else.
        (
            "self.plan",
            &["conductor/plan_gate.rs", "conductor/lifecycle.rs"],
        ),
        // The look-ahead's one predicate.
        ("has_paid_pin(", &["conductor/lifecycle.rs"]),
        // Who builds a plan.
        ("run_plan::build(", &["conductor/plan_gate.rs"]),
        // Who reads an approval of a chat, or takes an offer's.
        ("read_chat_approval(", &["conductor/plan_gate.rs"]),
        ("take_offer_approval(", &["conductor/plan_gate.rs"]),
        // Who reads the approval a run holds.
        (".plan.clone()", &["conductor/lifecycle.rs"]),
        ("ctx.plan", &["conductor/executor.rs"]),
        // Stage 8: who reads which home an approval is in (get_run_plan_approval), and who
        // writes, revokes or offers one. The five run plan commands, nowhere else.
        ("read_chat_approval_with_home(", &["commands/run_plan.rs"]),
        ("save_chat_approval(", &["commands/run_plan.rs"]),
        ("revoke_chat_approval(", &["commands/run_plan.rs"]),
        ("approve_offer(", &["commands/run_plan.rs"]),
        ("register_offer(", &["commands/run_plan.rs"]),
        // The memory home is reached by the gate, the clearing sites and main.rs only.
        (
            "PlanMemory",
            &[
                "conductor/plan_gate.rs",
                "conductor/lifecycle.rs",
                "commands/execution.rs",
                "commands/messages.rs",
                "commands/api_chat.rs",
                "commands/chats.rs",
                "commands/turn_marker.rs",
                "commands/run_plan.rs",
                "main.rs",
            ],
        ),
    ];
    let mut violations = Vec::new();
    walk(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
        &mut |name, text| {
            if skip(name) {
                return;
            }
            let code: String = code_of(text)
                .lines()
                .filter(|l| !l.trim_start().starts_with("//"))
                .collect::<Vec<_>>()
                .join("\n");
            for (needle, allowed) in rules {
                if has_path(&code, needle) && !allowed.iter().any(|a| name.ends_with(a)) {
                    violations.push(format!("{name}: {needle}"));
                }
            }
        },
    );
    assert!(violations.is_empty(), "{violations:#?}");
}

/// run_plan::effective_access is the rule lifecycle applies inline per step. Pinned two
/// ways: the function against an independent copy of the inline rule over every input, and a
/// scan that the inline expression is still in lifecycle.rs exactly (so a change there fails
/// here and forces the two back in step).
#[test]
fn the_plans_effective_access_is_the_rule_lifecycle_applies() {
    use crate::conductor::run_plan::effective_access;
    let levels = [
        ExternalAccess::LocalOnly,
        ExternalAccess::AnonymousRequired,
        ExternalAccess::AnonymousPreferred,
        ExternalAccess::Unrestricted,
    ];
    for focus in levels {
        for over in std::iter::once(None).chain(levels.iter().copied().map(Some)) {
            let mut s = step("s");
            s.external_access_override = over;
            // lifecycle.rs's rule, copied.
            let inline = s
                .external_access_override
                .map(|o| o.min(focus))
                .unwrap_or(focus);
            assert_eq!(effective_access(&s, focus), inline, "{focus:?} {over:?}");
        }
    }
    let lifecycle = squash(&code_of(include_str!("lifecycle.rs")));
    assert!(lifecycle.contains(&squash(
        ".external_access_override.map(|o| o.min(self._focus_external_access)).unwrap_or(self._focus_external_access)"
    )));
    let plan = squash(&code_of(include_str!("run_plan.rs")));
    assert!(plan.contains(&squash(
        "step.external_access_override.map(|o| o.min(focus_ceiling)).unwrap_or(focus_ceiling)"
    )));
}
