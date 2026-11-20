// src-tauri/src/conductor/run_park_lifecycle_tests.rs
//
// items.id=901 stage 7: the run's park reason, written by handle_step_failure in the same
// function that writes the run's focus_runs row (write_run_row) and cleared when the run moves
// on, against a real outputs.db under a scratch data root. The pure part (which failures leave
// a park, the shape filter, the trim) is in run_park_tests.rs.

use std::collections::{HashMap, HashSet};

use serde_json::{json, Value};

use super::*;
use crate::conductor::concurrency::ConductorScheduler;
use crate::conductor::failure::{FailureAction, FailureResult, FailureSeverity};
use crate::conductor::plan_memory::PlanMemory;
use crate::conductor::run_park::ParkRecord;
use crate::conductor::step_review::{park_result, BlockReason, Park};
use crate::persistence::output_store::{self, open_outputs_db_retrying};
use crate::persistence::run_park_store::get_park;
use crate::spend::codes::Code;
use crate::spend::gate::{block_metadata, SpendBlock};

const USER: &str = "u";
const PERSONA: &str = "p";
const KEY: &str = "1122334455667788112233445566778811223344556677881122334455667788";
const OTHER_KEY: &str = "aabbccddeeff00112233445566778899aabbccddeeff00112233445566778899";

async fn with_root<F: std::future::Future>(f: F) -> F::Output {
    let _lock = crate::test_support::ENV_MUTEX.lock().await;
    let saved = std::env::var("QR_DATA_ROOT").ok();
    let dir = tempfile::tempdir().expect("tempdir");
    std::env::set_var("QR_DATA_ROOT", dir.path());
    let out = f.await;
    match saved {
        Some(v) => std::env::set_var("QR_DATA_ROOT", v),
        None => std::env::remove_var("QR_DATA_ROOT"),
    }
    out
}

fn run_with(key: &str, memory: Option<PlanMemory>) -> FocusRun {
    let mut run: FocusRun = FocusRun::new(
        USER.to_owned(),
        PERSONA.to_owned(),
        "f".to_owned(),
        sqlx::SqlitePool::connect_lazy("sqlite:///qr-901-no-such-dir/none.db").unwrap(),
        std::sync::Arc::new(ConductorScheduler::new()),
        String::new(),
        false,
        Some(key.to_owned()),
        None,
        false,
        HashSet::new(),
        None,
    );
    run.focus_run_id = Some(uuid::Uuid::new_v4().to_string());
    let mut personal = PersonalTrack::new();
    personal.seal();
    run.personal_track = Some(personal);
    run.task_track = Some(TaskTrack::new());
    run.shared_state = Some(SharedStateTrack::new());
    run.plan_memory = memory;
    run
}

fn run_id(run: &FocusRun) -> String {
    run.focus_run_id.clone().unwrap()
}

async fn conn() -> sqlx::SqliteConnection {
    open_outputs_db_retrying(USER, PERSONA, KEY)
        .await
        .expect("outputs.db opens under the scratch root")
}

fn failure(meta: Option<HashMap<String, Value>>) -> FailureResult {
    FailureResult {
        action: FailureAction::AwaitUser,
        failure_mode: Some("F10".to_owned()),
        plain_language: "spend_block.limit_reached".to_owned(),
        is_recoverable: true,
        severity: FailureSeverity::Require,
        action_id: Some("draft".to_owned()),
        focus_id: Some("f".to_owned()),
        metadata: meta,
    }
}

fn from_block(block: &SpendBlock) -> FailureResult {
    let Value::Object(m) = block_metadata(block) else {
        unreachable!()
    };
    failure(Some(m.into_iter().collect()))
}

fn too_weak(n: usize) -> FailureResult {
    let opts: Vec<Value> = (0..n)
        .map(|i| json!({"subject":"local","model_id":format!("m-{i}"),"standing":"not_measured"}))
        .collect();
    from_block(
        &SpendBlock::new(Code::ResolverTooWeak)
            .with("required", "standard")
            .with("options", Value::Array(opts))
            .with("options_total", n),
    )
}

fn families() -> Vec<(&'static str, FailureResult)> {
    vec![
        ("resolver", too_weak(3)),
        (
            "resolver_none",
            from_block(&SpendBlock::new(Code::ResolverNoCandidate).with("reason", "none_ready")),
        ),
        (
            "spend",
            from_block(&SpendBlock::new(Code::BlockCapRequired).with("provider_id", "prov-1")),
        ),
        (
            "plan",
            from_block(
                &SpendBlock::new(Code::PlanExceeded)
                    .with("action_id", "draft")
                    .with("reason", "no_approval"),
            ),
        ),
        (
            "step_review",
            park_result(&Park::Blocked(BlockReason::Timeout), "draft", "f"),
        ),
    ]
}

async fn has_park(id: &str) -> bool {
    let mut c = conn().await;
    get_park(&mut c, id).await.unwrap().is_some()
}

async fn parks_without_a_row(c: &mut sqlx::SqliteConnection) -> i64 {
    sqlx::query_scalar(
        "SELECT COUNT(*) FROM run_parks p
         WHERE NOT EXISTS (SELECT 1 FROM focus_runs r WHERE r.id = p.run_id AND r.status = 'awaiting_user')",
    )
    .fetch_one(c)
    .await
    .unwrap()
}

async fn park_count(c: &mut sqlx::SqliteConnection) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM run_parks")
        .fetch_one(c)
        .await
        .unwrap()
}

// -- each family writes its park with the run's row --------------------------------------

#[tokio::test]
async fn each_family_parks_the_run_and_stores_its_reason_with_the_row() {
    with_root(async {
        for (name, f) in families() {
            let mut run = run_with(KEY, None);
            let expected = crate::conductor::run_park::from_failure(&f).expect(name);
            let r = run.handle_step_failure(f).await.unwrap();
            assert_eq!(r.status, "awaiting_user", "{name}");
            let mut c = conn().await;
            let got = get_park(&mut c, &run_id(&run)).await.unwrap().expect(name);
            assert_eq!(got.code, expected.code.as_str(), "{name}");
            assert_eq!(got.action_id, "draft");
            assert_eq!(got.params_json, expected.params_json, "{name}");
            // The park is written exactly when the run's own row is, at the parked status.
            assert_eq!(parks_without_a_row(&mut c).await, 0, "{name}");
        }
    })
    .await;
}

#[tokio::test]
async fn a_too_weak_park_carries_the_resolvers_options() {
    with_root(async {
        let mut run = run_with(KEY, None);
        run.handle_step_failure(too_weak(3)).await.unwrap();
        let mut c = conn().await;
        let p = get_park(&mut c, &run_id(&run)).await.unwrap().unwrap();
        let v: Value = serde_json::from_str(&p.params_json).unwrap();
        assert_eq!(v["required"], "standard");
        assert_eq!(v["options"].as_array().unwrap().len(), 3);
        assert_eq!(v["options"][0]["model_id"], "m-0");
        assert_eq!(v["options_total"], 3);
    })
    .await;
}

#[tokio::test]
async fn a_step_review_block_park_carries_its_reason_code() {
    with_root(async {
        let mut run = run_with(KEY, None);
        run.handle_step_failure(park_result(
            &Park::Blocked(BlockReason::FilterBlocked),
            "draft",
            "f",
        ))
        .await
        .unwrap();
        let mut c = conn().await;
        let p = get_park(&mut c, &run_id(&run)).await.unwrap().unwrap();
        assert_eq!(p.code, "step_review.blocked");
        assert_eq!(
            serde_json::from_str::<Value>(&p.params_json).unwrap()["reason"],
            "filter_blocked"
        );
    })
    .await;
}

// -- nothing else is stored, and what is stored is no text -------------------------------

#[tokio::test]
async fn a_failure_with_no_registered_park_code_stores_nothing_and_is_returned_whole() {
    with_root(async {
        let cases: Vec<FailureResult> = vec![
            failure(None),
            failure(Some(
                [(
                    "code".to_owned(),
                    json!("Something went wrong for Jane Doe"),
                )]
                .into(),
            )),
            failure(Some(
                [("code".to_owned(), json!("resolver.invented"))].into(),
            )),
            failure(Some(
                [("code".to_owned(), json!(Code::WarnCostMetered.as_str()))].into(),
            )),
            failure(Some(
                [(
                    "code".to_owned(),
                    json!(Code::ApiChatCeilingBlocked.as_str()),
                )]
                .into(),
            )),
        ];
        for f in cases {
            let before = serde_json::to_value(&f).unwrap();
            let mut run = run_with(KEY, None);
            let r = run.handle_step_failure(f).await.unwrap();
            assert_eq!(r.status, "awaiting_user");
            assert_eq!(
                serde_json::to_value(r.failure.as_ref().unwrap()).unwrap(),
                before
            );
            let mut c = conn().await;
            assert_eq!(park_count(&mut c).await, 0);
        }
    })
    .await;
}

#[tokio::test]
async fn no_row_of_run_parks_holds_text() {
    with_root(async {
        let prose = "Jane Doe lives at 5 Elm Street and her card ends 4242";
        let mut f = from_block(
            &SpendBlock::new(Code::BlockLimitReached)
                .with("limit_id", "lim-1")
                .with("spent_micros", 500)
                .with("note", prose)
                .with("detail", "two words")
                .with("nested", json!({"ok": "id-2", "text": prose})),
        );
        f.plain_language = prose.to_owned();
        let mut run = run_with(KEY, None);
        let r = run.handle_step_failure(f).await.unwrap();
        // The failure itself is untouched: only the stored row is filtered.
        assert_eq!(r.failure.unwrap().plain_language, prose);
        let mut c = conn().await;
        let rows: Vec<(String, String, String, String, String)> = sqlx::query_as(
            "SELECT run_id, action_id, code, params_json, created_at FROM run_parks",
        )
        .fetch_all(&mut c)
        .await
        .unwrap();
        assert_eq!(rows.len(), 1);
        let (run_id_, action, code, params, _) = &rows[0];
        for col in [run_id_, action, code, params] {
            assert!(!col.contains("Jane") && !col.contains("Elm") && !col.contains("4242"));
        }
        assert!(crate::persistence::run_park_store::is_registered_code(code));
        assert!(!params.contains(' ') && !params.contains('\n'));
        let v: Value = serde_json::from_str(params).unwrap();
        assert_eq!(v["limit_id"], "lim-1");
        assert_eq!(v["nested"]["ok"], "id-2");
        assert!(v.get("note").is_none() && v.get("detail").is_none());
        assert!(v["nested"].get("text").is_none());
    })
    .await;
}

// -- a park that cannot be stored never changes the failure ------------------------------

#[tokio::test]
async fn a_failed_park_write_leaves_the_original_failure_and_the_row_intact() {
    with_root(async {
        let memory = PlanMemory::default();
        let mut run = run_with(KEY, Some(memory.clone()));
        // The table is gone, so the park put fails; the run's own row still writes.
        let mut c = conn().await;
        sqlx::query("DROP TABLE run_parks")
            .execute(&mut c)
            .await
            .unwrap();
        drop(c);

        let f = too_weak(2);
        let before = serde_json::to_value(&f).unwrap();
        let r = run.handle_step_failure(f).await.unwrap();
        assert_eq!(r.status, "awaiting_user", "the row was written");
        assert_eq!(
            serde_json::to_value(r.failure.as_ref().unwrap()).unwrap(),
            before
        );
        assert_eq!(r.failure.as_ref().unwrap().action, FailureAction::AwaitUser);
        // Held in memory instead, so a screen still has the reason.
        let held = memory
            .get_run_park(USER, PERSONA, &run_id(&run))
            .expect("held");
        assert_eq!(held.code, Code::ResolverTooWeak);
        assert_eq!(memory.park_count(), 1);
    })
    .await;
}

#[tokio::test]
async fn a_failed_park_write_with_no_memory_home_still_returns_the_failure() {
    with_root(async {
        let mut run = run_with(KEY, None);
        let mut c = conn().await;
        sqlx::query("DROP TABLE run_parks")
            .execute(&mut c)
            .await
            .unwrap();
        drop(c);
        let f = too_weak(1);
        let before = serde_json::to_value(&f).unwrap();
        let r = run.handle_step_failure(f).await.unwrap();
        assert_eq!(r.status, "awaiting_user");
        assert_eq!(
            serde_json::to_value(r.failure.as_ref().unwrap()).unwrap(),
            before
        );
    })
    .await;
}

// -- the park follows the run's row (the latch tie) --------------------------------------

#[tokio::test]
async fn a_run_row_that_cannot_be_written_sends_the_park_to_memory_and_the_table_gets_none() {
    with_root(async {
        // The database exists under KEY; this run holds another key, so its row cannot be
        // written (the stand-in for any future change that suppresses the row).
        drop(conn().await);
        let memory = PlanMemory::default();
        let mut run = run_with(OTHER_KEY, Some(memory.clone()));
        let f = too_weak(2);
        let before = serde_json::to_value(&f).unwrap();
        let r = run.handle_step_failure(f).await.unwrap();
        assert_eq!(
            serde_json::to_value(r.failure.as_ref().unwrap()).unwrap(),
            before
        );

        let mut c = conn().await;
        assert_eq!(
            park_count(&mut c).await,
            0,
            "no persisted park without its row"
        );
        let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM focus_runs")
            .fetch_one(&mut c)
            .await
            .unwrap();
        assert_eq!(rows, 0, "and no row either");
        assert!(memory
            .get_run_park(USER, PERSONA, &run_id(&run))
            .is_some_and(|p| p.code == Code::ResolverTooWeak));
    })
    .await;
}

#[tokio::test]
async fn a_chat_run_in_a_latched_chat_keeps_its_row_and_so_its_park() {
    with_root(async {
        // The latch suppresses only the run's input (write_run_row), never the row, so the
        // park follows the row in a one-time chat exactly as the row does.
        let mut c = conn().await;
        crate::persistence::session_store::latch_write_suppressed(&mut c, "chat-1", "f", "r0")
            .await
            .unwrap();
        drop(c);
        let mut run = run_with(KEY, None);
        run.context_key = Some("chat-1".to_owned());
        run.handle_step_failure(too_weak(1)).await.unwrap();
        let mut c = conn().await;
        let status: String = sqlx::query_scalar("SELECT status FROM focus_runs WHERE id = ?")
            .bind(run_id(&run))
            .fetch_one(&mut c)
            .await
            .unwrap();
        assert_eq!(status, "awaiting_user");
        assert!(get_park(&mut c, &run_id(&run)).await.unwrap().is_some());
        assert_eq!(parks_without_a_row(&mut c).await, 0);
    })
    .await;
}

// -- the park is cleared when the run moves on -------------------------------------------

#[tokio::test]
async fn a_park_goes_when_the_run_resumes_ends_or_pauses_for_another_reason() {
    with_root(async {
        let mut run = run_with(KEY, None);
        let id = run_id(&run);

        run.handle_step_failure(too_weak(1)).await.unwrap();
        assert!(has_park(&id).await);
        // Resumed past it.
        run.write_focus_run_record("running").await.unwrap();
        assert!(!has_park(&id).await, "resume clears it");

        // Parked again, then ended.
        run.handle_step_failure(too_weak(1)).await.unwrap();
        assert!(has_park(&id).await);
        for end in ["complete", "cancelled", "failed"] {
            run.handle_step_failure(too_weak(1)).await.unwrap();
            run.write_focus_run_record_logged(end).await;
            assert!(!has_park(&id).await, "{end} clears it");
        }

        // A later park of the same run replaces the earlier reason.
        run.handle_step_failure(too_weak(1)).await.unwrap();
        run.handle_step_failure(from_block(&SpendBlock::new(Code::BlockPriceUnknown)))
            .await
            .unwrap();
        let mut c = conn().await;
        assert_eq!(park_count(&mut c).await, 1);
        assert_eq!(
            get_park(&mut c, &id).await.unwrap().unwrap().code,
            "spend_block.price_unknown"
        );
        drop(c);

        // A pause for a reason that leaves no park removes the stale one.
        run.handle_step_failure(FailureResult {
            action: FailureAction::AwaitFloorConsent,
            failure_mode: Some("F6".to_owned()),
            plain_language: "Needs your input to continue.".to_owned(),
            is_recoverable: true,
            severity: FailureSeverity::Pause,
            action_id: Some("draft".to_owned()),
            focus_id: Some("f".to_owned()),
            metadata: None,
        })
        .await
        .unwrap();
        assert!(!has_park(&id).await, "a stale reason is never left");
    })
    .await;
}

#[tokio::test]
async fn cancelling_or_setting_a_status_on_a_parked_run_clears_its_park() {
    with_root(async {
        let mut a = run_with(KEY, None);
        let mut b = run_with(KEY, None);
        let mut keep = run_with(KEY, None);
        for r in [&mut a, &mut b, &mut keep] {
            r.handle_step_failure(too_weak(1)).await.unwrap();
        }
        output_store::cancel_focus_run(USER, PERSONA, KEY, &run_id(&a))
            .await
            .unwrap();
        output_store::set_focus_run_status(USER, PERSONA, KEY, &run_id(&b), "complete")
            .await
            .unwrap();
        // The parked status itself is not a move.
        output_store::set_focus_run_status(USER, PERSONA, KEY, &run_id(&keep), "awaiting_user")
            .await
            .unwrap();
        let mut c = conn().await;
        assert!(get_park(&mut c, &run_id(&a)).await.unwrap().is_none());
        assert!(get_park(&mut c, &run_id(&b)).await.unwrap().is_none());
        assert!(get_park(&mut c, &run_id(&keep)).await.unwrap().is_some());
    })
    .await;
}

#[tokio::test]
async fn a_failing_clear_leaves_the_cancel_and_the_status_write_succeeding() {
    with_root(async {
        let mut a = run_with(KEY, None);
        let mut b = run_with(KEY, None);
        for r in [&mut a, &mut b] {
            r.handle_step_failure(too_weak(1)).await.unwrap();
        }
        // The park table is gone, so every clear_park fails.
        let mut c = conn().await;
        sqlx::query("DROP TABLE run_parks")
            .execute(&mut c)
            .await
            .unwrap();
        drop(c);

        output_store::cancel_focus_run(USER, PERSONA, KEY, &run_id(&a))
            .await
            .expect("the cancel still succeeds");
        output_store::set_focus_run_status(USER, PERSONA, KEY, &run_id(&b), "complete")
            .await
            .expect("the status write still succeeds");

        let mut c = conn().await;
        for (r, want) in [(&a, "cancelled"), (&b, "complete")] {
            let status: String = sqlx::query_scalar("SELECT status FROM focus_runs WHERE id = ?")
                .bind(run_id(r))
                .fetch_one(&mut c)
                .await
                .unwrap();
            assert_eq!(status, want);
        }
        // The rest of the cancel still ran: the snapshots of the cancelled run are gone.
        let snaps: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM focus_run_snapshots WHERE focus_run_id = ?")
                .bind(run_id(&a))
                .fetch_one(&mut c)
                .await
                .unwrap();
        assert_eq!(snaps, 0);
    })
    .await;
}

#[test]
fn the_two_output_store_clears_are_best_effort_with_one_fixed_log_line() {
    let src = include_str!("../persistence/output_store.rs");
    let code = src.split("#[cfg(test)]\nmod tests {").next().unwrap();
    assert_eq!(
        code.matches("clear_park(").count(),
        1,
        "one call, in the helper"
    );
    assert_eq!(
        code.matches("clear_park_best_effort(").count(),
        4,
        "helper plus three callers (cancel_focus_run, set_focus_run_status, and items.id=681's expire_parked_run)"
    );
    let helper = code
        .split("async fn clear_park_best_effort(")
        .nth(1)
        .unwrap()
        .split("\n}\n")
        .next()
        .unwrap();
    assert!(helper.contains("log::warn!(\"output_store: the run's park reason was not cleared\")"));
    assert_eq!(helper.matches("log::").count(), 1);
}

#[tokio::test]
async fn a_held_park_goes_when_a_later_row_write_succeeds_and_at_logout() {
    with_root(async {
        let memory = PlanMemory::default();
        let mut run = run_with(OTHER_KEY, Some(memory.clone()));
        drop(conn().await);
        run.handle_step_failure(too_weak(1)).await.unwrap();
        assert_eq!(memory.park_count(), 1);
        // The key is right again: the next status write settles the park and drops the copy.
        run.key_hex = Some(KEY.to_owned());
        run.write_focus_run_record("running").await.unwrap();
        assert_eq!(memory.park_count(), 0);

        // And a held park never outlives the key.
        let mut again = run_with(OTHER_KEY, Some(memory.clone()));
        again.handle_step_failure(too_weak(1)).await.unwrap();
        assert_eq!(memory.park_count(), 1);
        memory.clear_user(USER);
        assert_eq!(memory.park_count(), 0);
    })
    .await;
}

// -- source scans ------------------------------------------------------------------------

fn production(src: &str) -> String {
    src.split("\n#[cfg(test)]\nmod ")
        .next()
        .unwrap()
        .split("\n#[cfg(test)]\n#[path")
        .next()
        .unwrap()
        .to_owned()
}

fn rust_files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
    for e in std::fs::read_dir(dir).unwrap().flatten() {
        let p = e.path();
        if p.is_dir() {
            rust_files(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
}

#[test]
fn the_park_is_written_in_one_place_and_only_with_the_row() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    rust_files(&root, &mut files);
    let mut callers = Vec::new();
    for f in files {
        let name = f.file_name().unwrap().to_string_lossy().into_owned();
        if name.ends_with("_tests.rs") {
            continue;
        }
        let src = std::fs::read_to_string(&f).unwrap();
        let code = production(&src);
        if code.contains("put_park(") && !name.ends_with("run_park_store.rs") {
            callers.push(name);
        }
    }
    assert_eq!(callers, vec!["lifecycle.rs".to_owned()], "one writer file");

    let life = production(include_str!("lifecycle.rs"));
    assert_eq!(life.matches("put_park(").count(), 1);
    assert_eq!(life.matches("clear_park(").count(), 1);
    // Both are inside settle_park, which only write_run_row calls.
    let settle = life
        .split("async fn settle_park(")
        .nth(1)
        .unwrap()
        .split("fn hold_park_in_memory(")
        .next()
        .unwrap();
    assert!(settle.contains("put_park(") && settle.contains("clear_park("));
    assert_eq!(life.matches("self.settle_park(").count(), 1);
    let row = life
        .split("async fn write_run_row(")
        .nth(1)
        .unwrap()
        .split("async fn settle_park(")
        .next()
        .unwrap();
    assert!(row.contains("self.settle_park("));
    // The park is built in handle_step_failure and held in memory only there.
    assert_eq!(life.matches("self.hold_park_in_memory(").count(), 1);
    let handler = life
        .split("async fn handle_step_failure(")
        .nth(1)
        .unwrap()
        .split("async fn get_current_status(")
        .next()
        .unwrap();
    assert!(handler.contains("run_park::from_failure(&failure)"));
    assert!(handler.contains("write_focus_run_record_parked("));
    assert!(handler.contains("self.hold_park_in_memory("));
    assert!(
        !handler.contains("run_park_store"),
        "the store is behind write_run_row"
    );
    // The handler never rewrites the failure it returns.
    assert!(!handler.contains("failure ="));
    assert!(!handler.contains("mut failure"));
}

#[test]
fn the_park_log_lines_carry_no_parameter_and_no_text() {
    let life = production(include_str!("lifecycle.rs"));
    let settle = life
        .split("async fn settle_park(")
        .nth(1)
        .unwrap()
        .split("fn hold_park_in_memory(")
        .next()
        .unwrap();
    let hold = life
        .split("fn hold_park_in_memory(")
        .nth(1)
        .unwrap()
        .split("/// items.id=894 B0: whether this run's chat carries")
        .next()
        .unwrap();
    assert!(!hold.contains("log::"));
    for line in settle.lines().filter(|l| l.contains("log::")) {
        assert!(!line.contains('{') || line.contains("{}"), "{line}");
        assert!(
            !line.contains("params") && !line.contains("park."),
            "{line}"
        );
    }
    assert_eq!(settle.matches("log::warn!").count(), 2);
    for lit in ["not stored", "not cleared"] {
        assert!(settle.contains(lit));
    }
}

#[test]
fn a_held_park_is_a_record_with_no_text_and_the_debug_is_redacted() {
    let p = ParkRecord {
        code: Code::ResolverTooWeak,
        action_id: "draft".to_owned(),
        params_json: "{\"required\":\"standard\"}".to_owned(),
    };
    assert_eq!(format!("{p:?}"), "ParkRecord(resolver.too_weak)");
    let m = PlanMemory::default();
    m.put_run_park("u", "p", "r", p);
    let shown = format!("{m:?}");
    assert!(!shown.contains("standard") && !shown.contains("draft"));
}
