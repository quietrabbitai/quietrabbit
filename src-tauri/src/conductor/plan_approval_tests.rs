// src-tauri/src/conductor/plan_approval_tests.rs
//
// Tests of conductor::plan_approval (items.id=901 stage 3): making an approval from the
// plan the user was shown, checking a stored one against the plan the run recomputes, the
// two homes (persisted for an unlatched chat, memory only for a latched or unreadable one),
// revocation, and the chatless token flow.

use super::*;
use crate::conductor::model_block::config::{Destination, QualityBar};
use crate::conductor::model_capability::AttainedBar;
use crate::conductor::model_resolver::Readiness;
use crate::conductor::plan_memory::OfferOwner;
use crate::conductor::run_plan::tests::{
    cell, hosted_cand, hosted_req, inputs_for, launch, local_cand, plan_of, planned, requirement,
};
use crate::conductor::run_plan::{OfferStrategy, Pin, RunPlan};
use crate::conductor::tokens::ExternalAccess;
use crate::persistence::session_store::{latch_write_suppressed, test_db::mem_outputs_conn};

const USER: &str = "user-1";
const PERSONA: &str = "persona-1";
const CHAT: &str = "chat-1";
const FOCUS: &str = "fixture-focus";

fn scope() -> ChatScope {
    ChatScope {
        user_id: USER.to_owned(),
        persona_id: PERSONA.to_owned(),
        context_key: CHAT.to_owned(),
        focus_id: FOCUS.to_owned(),
    }
}

/// One hosted-capable step with a qualifying local model, an unmeasured local model, an
/// unmeasured hosted model priced at 500 and two hosted models that cannot be selected.
fn plan_with(est: u64) -> RunPlan {
    let l_ok = local_cand("l-basic");
    let l_new = local_cand("l-unmeasured");
    let h_ok = hosted_cand("host-a", "h-unmeasured", est);
    let h_none = hosted_cand("host-a", "h-no-estimate", u64::MAX);
    let h_low = hosted_cand("host-b", "h-below", 100);
    let caps = [
        cell(&l_ok, AttainedBar::Basic),
        cell(&h_low, AttainedBar::BelowBasic),
    ];
    plan_of(
        vec![planned("draft", hosted_req())],
        &launch(None),
        &inputs_for(&["draft"], &[l_ok, l_new, h_ok, h_none, h_low], &caps),
    )
    .unwrap()
}

fn pin(strategy: OfferStrategy, provider: &str, model: &str) -> Pin {
    Pin {
        action_id: "draft".to_owned(),
        strategy,
        provider_id: provider.to_owned(),
        model_id: model.to_owned(),
    }
}

fn paid() -> Pin {
    pin(OfferStrategy::PaidApi, "host-a", "h-unmeasured")
}

// ---------------------------------------------------------------------------
// approve
// ---------------------------------------------------------------------------

#[test]
fn an_approval_is_made_from_the_shown_pin_free_digest_and_stores_the_digest_with_its_pins() {
    let plan = plan_with(500);
    // No pins: the digest is the pin-free plan digest.
    let none = approve(&plan, &plan.digest, 0, vec![]).unwrap();
    assert_eq!(none.digest, plan.digest);
    // A paid pin: the stored digest is the pin-free digest with the pin folded in.
    let a = approve(&plan, &plan.digest, 500, vec![paid()]).unwrap();
    assert_eq!(a.digest, plan.digest_with(&[paid()]));
    assert_ne!(a.digest, plan.digest);
    assert_eq!((a.ceiling_micros, a.pins.clone()), (500, vec![paid()]));
    // An unmeasured local model can be pinned too, at no cost.
    let local = pin(OfferStrategy::LocalUnmeasured, "", "l-unmeasured");
    assert!(approve(&plan, &plan.digest, 0, vec![local]).is_ok());
}

#[test]
fn a_changed_plan_approves_nothing() {
    let plan = plan_with(500);
    assert_eq!(
        approve(&plan, &"0".repeat(64), 500, vec![]),
        Err(ApprovalError::PlanChanged)
    );
    // The digest after a pin is not the digest the user was shown.
    assert_eq!(
        approve(&plan, &plan.digest_with(&[paid()]), 500, vec![paid()]),
        Err(ApprovalError::PlanChanged)
    );
    // A plan with a different range has another digest.
    let other = plan_of(
        vec![planned(
            "draft",
            requirement(
                QualityBar::Standard,
                QualityBar::High,
                &[Destination::Local, Destination::HostedApi],
                ExternalAccess::AnonymousPreferred,
            ),
        )],
        &launch(None),
        &inputs_for(&["draft"], &[], &[]),
    )
    .unwrap();
    assert_eq!(
        approve(&plan, &other.digest, 500, vec![]),
        Err(ApprovalError::PlanChanged)
    );
}

#[test]
fn the_ceiling_must_cover_the_estimate_and_stay_under_the_maximum() {
    let plan = plan_with(500);
    assert_eq!(
        approve(&plan, &plan.digest, 499, vec![paid()]),
        Err(ApprovalError::CeilingBelowEstimate)
    );
    assert!(approve(&plan, &plan.digest, 500, vec![paid()]).is_ok());
    assert!(approve(&plan, &plan.digest, PLAN_CEILING_MAX_MICROS, vec![paid()]).is_ok());
    assert_eq!(
        approve(
            &plan,
            &plan.digest,
            PLAN_CEILING_MAX_MICROS + 1,
            vec![paid()]
        ),
        Err(ApprovalError::CeilingTooHigh)
    );
    assert_eq!(PLAN_CEILING_MAX_MICROS, 25_000_000);
}

#[test]
fn only_a_selectable_offer_of_an_approvable_kind_can_be_pinned() {
    let plan = plan_with(500);
    let try_pin = |p: Pin| approve(&plan, &plan.digest, 1_000, vec![p]);
    assert_eq!(
        try_pin(Pin {
            action_id: "nope".into(),
            ..paid()
        }),
        Err(ApprovalError::UnknownStep)
    );
    // A model the resolver would pick anyway needs no pin.
    assert_eq!(
        try_pin(pin(OfferStrategy::Local, "", "l-basic")),
        Err(ApprovalError::PinNotAllowed)
    );
    // Not one of the step's offers.
    assert_eq!(
        try_pin(pin(OfferStrategy::PaidApi, "host-a", "not-offered")),
        Err(ApprovalError::PinNotOffered)
    );
    // Right model, wrong kind.
    assert_eq!(
        try_pin(pin(
            OfferStrategy::LocalUnmeasured,
            "host-a",
            "h-unmeasured"
        )),
        Err(ApprovalError::PinNotOffered)
    );
    // Offered but not selectable: no estimate, and below the bar.
    assert_eq!(
        try_pin(pin(OfferStrategy::PaidApi, "host-a", "h-no-estimate")),
        Err(ApprovalError::NotSelectable)
    );
    assert_eq!(
        try_pin(pin(OfferStrategy::PaidApi, "host-b", "h-below")),
        Err(ApprovalError::NotSelectable)
    );
    // One choice per step.
    assert_eq!(
        approve(
            &plan,
            &plan.digest,
            1_000,
            vec![
                paid(),
                pin(OfferStrategy::LocalUnmeasured, "", "l-unmeasured")
            ]
        ),
        Err(ApprovalError::DuplicatePin)
    );
}

// ---------------------------------------------------------------------------
// verify: the run's own plan is the only input
// ---------------------------------------------------------------------------

#[test]
fn a_stored_approval_is_checked_against_the_plan_the_run_recomputed() {
    let plan = plan_with(500);
    let a = approve(&plan, &plan.digest, 500, vec![paid()]).unwrap();
    assert!(verify(&plan, &a));

    // What is installed, ready or priced today is not part of the digest.
    let mut cheaper = plan_with(5);
    assert!(verify(&cheaper, &a));
    cheaper = plan_with(9_999);
    assert!(verify(&cheaper, &a));

    // A different structure is not the plan that was approved.
    let narrower = plan_of(
        vec![planned(
            "draft",
            crate::conductor::run_plan::tests::local_req(),
        )],
        &launch(None),
        &inputs_for(&["draft"], &[], &[]),
    )
    .unwrap();
    assert!(!verify(&narrower, &a));

    // Changed pins under the same digest, or a changed digest under the same pins, fail:
    // the digest is recomputed from the pins, never taken on trust.
    let mut tampered = a.clone();
    tampered.pins = vec![pin(OfferStrategy::PaidApi, "host-a", "another-model")];
    assert!(!verify(&plan, &tampered));
    let mut forged = a.clone();
    forged.digest = plan.digest.clone();
    assert!(
        !verify(&plan, &forged),
        "the pin-free digest does not cover a pin"
    );
}

#[test]
fn readiness_cannot_change_whether_an_approval_still_holds() {
    let plan = plan_with(500);
    let a = approve(&plan, &plan.digest, 500, vec![paid()]).unwrap();
    // The hosted model becomes unready: the approval still describes the same plan.
    let l_ok = local_cand("l-basic");
    let mut h = hosted_cand("host-a", "h-unmeasured", 500);
    h.readiness = Readiness::NoKey;
    let unready = plan_of(
        vec![planned("draft", hosted_req())],
        &launch(None),
        &inputs_for(
            &["draft"],
            &[l_ok.clone(), h],
            &[cell(&l_ok, AttainedBar::Basic)],
        ),
    )
    .unwrap();
    assert!(verify(&unready, &a));
}

// ---------------------------------------------------------------------------
// choices_json
// ---------------------------------------------------------------------------

#[test]
fn pins_round_trip_through_the_stored_choices_and_an_unreadable_choice_is_an_error() {
    let pins = vec![
        paid(),
        pin(OfferStrategy::LocalUnmeasured, "", "l-unmeasured"),
    ];
    let json = pins_to_json(&pins).unwrap();
    assert_eq!(pins_from_json(&json).unwrap(), pins);
    assert_eq!(pins_from_json("[]").unwrap(), Vec::<Pin>::new());
    for bad in [
        "{}",
        "nope",
        r#"[{"action_id":"a"}]"#,
        r#"[{"action_id":"a","strategy":"local","provider_id":"","model_id":"m","extra":1}]"#,
        r#"[{"action_id":"a","strategy":"free_lunch","provider_id":"","model_id":"m"}]"#,
    ] {
        assert_eq!(pins_from_json(bad), Err(ApprovalError::BadChoices), "{bad}");
    }
}

// ---------------------------------------------------------------------------
// The two homes
// ---------------------------------------------------------------------------

fn an_approval() -> Approval {
    let plan = plan_with(500);
    approve(&plan, &plan.digest, 500, vec![paid()]).unwrap()
}

async fn rows(conn: &mut SqliteConnection) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM plan_approvals")
        .fetch_one(&mut *conn)
        .await
        .unwrap()
}

#[tokio::test]
async fn an_unlatched_chat_persists_its_approval_and_reads_it_back() {
    let mut conn = mem_outputs_conn().await;
    let mem = PlanMemory::default();
    let a = an_approval();
    let home = save_chat_approval(&mem, Some(&mut conn), &scope(), &a)
        .await
        .unwrap();
    assert_eq!(home, Home::Persisted { version: 1 });
    assert_eq!(rows(&mut conn).await, 1);
    assert_eq!(mem.approval_count(), 0, "nothing in memory");
    let back = read_chat_approval(&mem, Some(&mut conn), &scope())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(back, a, "digest, ceiling and pins survive the row");
    // A second approval replaces it.
    let home = save_chat_approval(&mem, Some(&mut conn), &scope(), &a)
        .await
        .unwrap();
    assert_eq!(home, Home::Persisted { version: 2 });
    // Another Focus and another chat see nothing.
    let mut other = scope();
    other.focus_id = "tech-support".into();
    assert!(read_chat_approval(&mem, Some(&mut conn), &other)
        .await
        .unwrap()
        .is_none());
}

/// Ruling R-A: a latched-chat submit writes no row, and the approval still holds, in memory.
#[tokio::test]
async fn a_latched_chat_keeps_its_approval_in_memory_and_writes_no_row() {
    let mut conn = mem_outputs_conn().await;
    latch_write_suppressed(&mut conn, CHAT, FOCUS, "run-a")
        .await
        .unwrap();
    let mem = PlanMemory::default();
    let a = an_approval();
    let home = save_chat_approval(&mem, Some(&mut conn), &scope(), &a)
        .await
        .unwrap();
    assert_eq!(home, Home::Memory);
    assert_eq!(rows(&mut conn).await, 0, "nothing was written");
    assert_eq!(mem.approval_count(), 1);
    assert_eq!(
        read_chat_approval(&mem, Some(&mut conn), &scope())
            .await
            .unwrap(),
        Some(a)
    );
}

/// An unreadable latch fails closed to the memory-only home, never to the persisted one.
#[tokio::test]
async fn an_unreadable_latch_fails_closed_to_memory() {
    let mut conn = mem_outputs_conn().await;
    sqlx::query("DROP TABLE session_variables")
        .execute(&mut conn)
        .await
        .unwrap();
    let mem = PlanMemory::default();
    let home = save_chat_approval(&mem, Some(&mut conn), &scope(), &an_approval())
        .await
        .unwrap();
    assert_eq!(home, Home::Memory);
    assert_eq!(rows(&mut conn).await, 0);
    assert_eq!(mem.approval_count(), 1);
}

/// A database that cannot be opened is a latch that cannot be read.
#[tokio::test]
async fn no_connection_means_memory_and_an_unreadable_read_is_never_permission() {
    let mem = PlanMemory::default();
    let a = an_approval();
    assert_eq!(
        save_chat_approval(&mem, None, &scope(), &a).await.unwrap(),
        Home::Memory
    );
    assert_eq!(
        read_chat_approval(&mem, None, &scope()).await.unwrap(),
        Some(a)
    );
    // Nothing in memory and no way to read the row: an error, which the gate reads as
    // "no approval".
    let empty = PlanMemory::default();
    assert_eq!(
        read_chat_approval(&empty, None, &scope()).await,
        Err(ApprovalError::Unreadable)
    );
}

/// Approvals written before a latch stay and still read. A new approval in the now latched
/// chat goes to memory and, being newer, wins.
#[tokio::test]
async fn an_approval_persisted_before_the_latch_still_reads_and_a_newer_memory_one_wins() {
    let mut conn = mem_outputs_conn().await;
    let mem = PlanMemory::default();
    let before = an_approval();
    save_chat_approval(&mem, Some(&mut conn), &scope(), &before)
        .await
        .unwrap();
    latch_write_suppressed(&mut conn, CHAT, FOCUS, "run-a")
        .await
        .unwrap();
    assert_eq!(
        read_chat_approval(&mem, Some(&mut conn), &scope())
            .await
            .unwrap(),
        Some(before.clone()),
        "setting the latch deleted nothing"
    );

    let plan = plan_with(500);
    let after = approve(&plan, &plan.digest, 900, vec![paid()]).unwrap();
    assert_eq!(
        save_chat_approval(&mem, Some(&mut conn), &scope(), &after)
            .await
            .unwrap(),
        Home::Memory
    );
    assert_eq!(rows(&mut conn).await, 1, "still only the earlier row");
    assert_eq!(
        read_chat_approval(&mem, Some(&mut conn), &scope())
            .await
            .unwrap()
            .unwrap()
            .ceiling_micros,
        900,
        "the newer, memory-only approval wins"
    );
}

/// Persisting an approval retires an older memory one, so one source of truth remains.
#[tokio::test]
async fn persisting_retires_an_older_memory_approval() {
    let mut conn = mem_outputs_conn().await;
    let mem = PlanMemory::default();
    save_chat_approval(&mem, None, &scope(), &an_approval())
        .await
        .unwrap();
    assert_eq!(mem.approval_count(), 1);
    save_chat_approval(&mem, Some(&mut conn), &scope(), &an_approval())
        .await
        .unwrap();
    assert_eq!(mem.approval_count(), 0);
}

/// Revocation reaches BOTH homes, and works in a latched chat.
#[tokio::test]
async fn revocation_removes_the_persisted_and_the_memory_approval_even_in_a_latched_chat() {
    let mut conn = mem_outputs_conn().await;
    let mem = PlanMemory::default();
    let a = an_approval();
    save_chat_approval(&mem, Some(&mut conn), &scope(), &a)
        .await
        .unwrap();
    latch_write_suppressed(&mut conn, CHAT, FOCUS, "run-a")
        .await
        .unwrap();
    save_chat_approval(&mem, Some(&mut conn), &scope(), &a)
        .await
        .unwrap();
    assert_eq!((rows(&mut conn).await, mem.approval_count()), (1, 1));

    let gone = revoke_chat_approval(&mem, Some(&mut conn), &scope())
        .await
        .unwrap();
    assert_eq!(
        gone,
        Revoked {
            memory: true,
            persisted: true
        }
    );
    assert_eq!((rows(&mut conn).await, mem.approval_count()), (0, 0));
    assert_eq!(
        read_chat_approval(&mem, Some(&mut conn), &scope())
            .await
            .unwrap(),
        None
    );
    // Nothing left to revoke.
    assert_eq!(
        revoke_chat_approval(&mem, Some(&mut conn), &scope())
            .await
            .unwrap(),
        Revoked {
            memory: false,
            persisted: false
        }
    );
    // Without a connection the memory home is still cleared, and the caller is told the
    // persisted one could not be reached.
    save_chat_approval(&mem, None, &scope(), &a).await.unwrap();
    assert_eq!(
        revoke_chat_approval(&mem, None, &scope()).await,
        Err(ApprovalError::Unavailable)
    );
    assert_eq!(mem.approval_count(), 0);
}

// ---------------------------------------------------------------------------
// A run with no chat: an opaque token, approved once, consumed with the run
// ---------------------------------------------------------------------------

#[test]
fn a_chatless_offer_is_approved_by_token_and_consumed_by_the_run_that_uses_it() {
    let mem = PlanMemory::default();
    let owner = OfferOwner {
        user_id: USER.to_owned(),
        persona_id: PERSONA.to_owned(),
        focus_id: FOCUS.to_owned(),
    };
    let plan = plan_with(500);
    // get_run_plan with no chat registers the offer it shows and returns the token.
    let token = mem.register_offer(&owner, &plan.digest).unwrap();
    // approve_run_plan recomputes the plan, builds the approval from the shown digest and
    // attaches it to the token. A changed plan approves nothing.
    let a = approve(&plan, &plan.digest, 500, vec![paid()]).unwrap();
    assert_eq!(
        mem.approve_offer(&token, &owner, &"0".repeat(64), a.clone()),
        Err(crate::conductor::plan_memory::OfferError::PlanChanged)
    );
    mem.approve_offer(&token, &owner, &plan.digest, a.clone())
        .unwrap();
    // The run's plan gate takes it (consuming it) and verifies it against its OWN plan.
    let taken = mem.take_offer_approval(&token, &owner).unwrap();
    assert!(verify(&plan_with(500), &taken));
    // A later run cannot reuse the token.
    assert!(mem.take_offer_approval(&token, &owner).is_none());
    // Nothing of this touched a database: the flow has no connection to write to.
}

// ---------------------------------------------------------------------------
// Scans
// ---------------------------------------------------------------------------

fn code_without_comments(src: &str) -> String {
    src.split("#[cfg(test)]")
        .next()
        .unwrap()
        .lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Nothing launch-related approves anything: the approval module names no launch value,
/// mode or answer (ruling R-A.6 and the plan's constraints).
#[test]
fn the_approval_module_names_no_launch_value() {
    let code = code_without_comments(include_str!("plan_approval.rs"));
    for banned in [
        "bound_spec",
        "launch_mode",
        "LaunchResolution",
        "run_settings",
        "launch_config",
        "document_type",
        "run_change",
    ] {
        assert!(!code.contains(banned), "'{banned}' in plan_approval.rs");
    }
}

/// put_approval has ONE caller in the crate: save_chat_approval, which hands it only an
/// approval the user made, after the latch check inside the store. The memory branches call
/// nothing of the persisted store.
#[test]
fn only_save_chat_approval_calls_the_persisted_writer() {
    fn walk(dir: std::path::PathBuf, f: &mut dyn FnMut(&std::path::Path, &str)) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(path, f);
            } else if path.extension().is_some_and(|e| e == "rs") {
                f(&path, &std::fs::read_to_string(&path).unwrap());
            }
        }
    }
    let mut callers = Vec::new();
    walk(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
        &mut |p, text| {
            let name = p.to_string_lossy().into_owned();
            if name.ends_with("persistence/plan_approval_store.rs") || name.ends_with("_tests.rs") {
                return;
            }
            let code = code_without_comments(text);
            let calls = code
                .lines()
                .filter(|l| l.contains("put_approval(") && !l.trim_start().starts_with("use "))
                .count();
            if calls > 0 {
                callers.push((name, calls));
            }
        },
    );
    assert_eq!(callers.len(), 1, "{callers:?}");
    assert!(callers[0].0.ends_with("conductor/plan_approval.rs"));
    assert_eq!(callers[0].1, 1);

    // The save function's persisted branch is the only place the store is written.
    let code = code_without_comments(include_str!("plan_approval.rs"));
    assert_eq!(code.matches("put_approval(").count(), 1);
    assert_eq!(code.matches("delete_approval(").count(), 1);
}
