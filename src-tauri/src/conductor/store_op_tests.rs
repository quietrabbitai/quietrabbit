// src-tauri/src/conductor/store_op_tests.rs
//
// items.id=887: the store-writer HANDLERS against REAL encrypted outputs.db
// files (a QR_DATA_ROOT scratch dir, real migrations, the production
// run_store_op and the production typed store functions underneath it).
//
// What is covered here: every operation's happy path and idempotent replay, the
// one-time latch refusal (including a row whose arguments would not resolve and
// a mis-authored row aimed at write_mode), focus_id scoping on case and delta
// writes, persona isolation, the fail-closed argument and store errors, and two
// source scans (no explicit `at`, no SQL of its own). LOAD validation and
// execute() integration are tested beside the loader, in lifecycle.rs.

use std::collections::HashMap;

use chrono::Utc;
use sqlx::SqliteConnection;

use crate::conductor::store_op::StoreOp;
use crate::conductor::store_writer::{
    run_store_op, Outcome, StoreWriteError, WriterContext, MAX_LIST_ITEMS,
};
use crate::persistence::case_store::{self, CaseState, CaseStoreError, DeltaStatus, EntityRole};
use crate::persistence::handles;
use crate::persistence::output_store::open_outputs_db;
use crate::persistence::session_store::{self, LatchError, SessionStoreError, WriteMode};

const KEY_HEX: &str = "deadbeef00112233445566778899aabbccddeeff00112233445566778899aabb";
const USER: &str = "u-writer";
const PERSONA: &str = "p-writer";
const CHAT: &str = "chat-w";
const FOCUS: &str = "tech-support";
const OTHER_FOCUS: &str = "other-focus";

struct TestRoot {
    _lock: tokio::sync::MutexGuard<'static, ()>,
    saved: Option<String>,
    _dir: tempfile::TempDir,
}

impl TestRoot {
    async fn new() -> Self {
        let lock = crate::test_support::ENV_MUTEX.lock().await;
        let saved = std::env::var("QR_DATA_ROOT").ok();
        let dir = tempfile::tempdir().expect("tempdir");
        std::env::set_var("QR_DATA_ROOT", dir.path());
        TestRoot {
            _lock: lock,
            saved,
            _dir: dir,
        }
    }
}

impl Drop for TestRoot {
    fn drop(&mut self) {
        match &self.saved {
            Some(v) => std::env::set_var("QR_DATA_ROOT", v),
            None => std::env::remove_var("QR_DATA_ROOT"),
        }
    }
}

async fn open(persona: &str) -> SqliteConnection {
    open_outputs_db(USER, persona, KEY_HEX)
        .await
        .expect("open persona outputs.db")
}

fn op(name: &str, target: Option<&str>, args: Option<serde_json::Value>) -> StoreOp {
    let args = args.map(|a| a.to_string());
    StoreOp::from_columns(Some(name), target, args.as_deref())
        .expect("row parses")
        .expect("a writer")
}

fn vars(pairs: &[(&str, &str)]) -> HashMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect()
}

fn ctx<'a>(focus: &'a str, run: &'a str, action: &'a str) -> WriterContext<'a> {
    WriterContext {
        context_key: Some(CHAT),
        focus_id: focus,
        run_id: run,
        action_id: action,
    }
}

async fn run(
    conn: &mut SqliteConnection,
    c: &WriterContext<'_>,
    o: &StoreOp,
    v: &HashMap<String, String>,
) -> Result<crate::conductor::store_writer::Done, StoreWriteError> {
    run_store_op(conn, c, o, v).await
}

/// case_open then return the case id it published.
async fn open_a_case(conn: &mut SqliteConnection, focus: &str, run_id: &str) -> String {
    let o = op(
        "case_open",
        Some("new_case"),
        Some(serde_json::json!({"entity": "e", "label": "l", "symptoms": "s"})),
    );
    let d = run(
        conn,
        &ctx(focus, run_id, "open"),
        &o,
        &vars(&[
            ("e", "ent-router"),
            ("l", "Router"),
            ("s", "drops at night"),
        ]),
    )
    .await
    .expect("case_open");
    assert_eq!(d.outcome, Outcome::Applied);
    d.published.expect("published the case id").1
}

async fn count(conn: &mut SqliteConnection, table: &str) -> i64 {
    sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {table}"))
        .fetch_one(&mut *conn)
        .await
        .unwrap()
}

// ---------------------------------------------------------------------------
// Each operation, against real encrypted files
// ---------------------------------------------------------------------------

#[tokio::test]
async fn session_put_creates_updates_and_replays_through_the_typed_store() {
    let _root = TestRoot::new().await;
    let mut conn = open(PERSONA).await;
    let o = op(
        "session_put",
        Some("locked_intent"),
        Some(serde_json::json!({"value": "intent_var"})),
    );

    let d = run(
        &mut conn,
        &ctx(FOCUS, "run-1", "put"),
        &o,
        &vars(&[("intent_var", "  billing\n")]),
    )
    .await
    .unwrap();
    assert_eq!(d.outcome, Outcome::Applied);
    assert_eq!(d.published, None);
    let row = session_store::get_session_variable(&mut conn, CHAT, FOCUS, "locked_intent")
        .await
        .unwrap()
        .unwrap();
    assert_eq!((row.value.as_str(), row.version), ("billing", 1));
    assert_eq!(row.written_by_run, "run-1");

    // A new value moves the version through the compare-and-set.
    let d = run(
        &mut conn,
        &ctx(FOCUS, "run-2", "put"),
        &o,
        &vars(&[("intent_var", "network")]),
    )
    .await
    .unwrap();
    assert_eq!(d.outcome, Outcome::Applied);
    let row = session_store::get_session_variable(&mut conn, CHAT, FOCUS, "locked_intent")
        .await
        .unwrap()
        .unwrap();
    assert_eq!((row.value.as_str(), row.version), ("network", 2));

    // The same value again writes nothing.
    let d = run(
        &mut conn,
        &ctx(FOCUS, "run-3", "put"),
        &o,
        &vars(&[("intent_var", "network")]),
    )
    .await
    .unwrap();
    assert_eq!(d.outcome, Outcome::AlreadyApplied);
    let row = session_store::get_session_variable(&mut conn, CHAT, FOCUS, "locked_intent")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.version, 2);
}

#[tokio::test]
async fn the_latch_operation_sets_the_latch_once_and_is_not_a_failure_to_repeat() {
    let _root = TestRoot::new().await;
    let mut conn = open(PERSONA).await;
    let o = op("latch", None, None);
    assert_eq!(
        session_store::write_mode(&mut conn, CHAT).await.unwrap(),
        WriteMode::Open
    );
    let d = run(&mut conn, &ctx(FOCUS, "run-1", "latch"), &o, &vars(&[]))
        .await
        .unwrap();
    assert_eq!(d.outcome, Outcome::Applied);
    assert_eq!(
        session_store::write_mode(&mut conn, CHAT).await.unwrap(),
        WriteMode::Suppressed
    );
    let d = run(&mut conn, &ctx(FOCUS, "run-2", "latch"), &o, &vars(&[]))
        .await
        .unwrap();
    assert_eq!(d.outcome, Outcome::AlreadyApplied);
}

#[tokio::test]
async fn case_open_publishes_a_run_derived_id_and_a_replay_writes_nothing() {
    let _root = TestRoot::new().await;
    let mut conn = open(PERSONA).await;
    let id = open_a_case(&mut conn, FOCUS, "run-1").await;
    assert_eq!(id, "run-1:open");
    let case = case_store::get_case(&mut conn, &id).await.unwrap().unwrap();
    assert_eq!(case.focus_id, FOCUS, "the running Focus's id, not a row's");
    assert_eq!(case.state, CaseState::Open);
    assert_eq!(case.current_rung, 1);
    assert_eq!(case.primary_entity_id.as_deref(), Some("ent-router"));
    assert_eq!(case.subject_label, "Router");
    assert_eq!(case.opened_by_run, "run-1");

    // The same row in the same run again is Existing.
    let o = op(
        "case_open",
        Some("new_case"),
        Some(serde_json::json!({"entity": "e", "label": "l"})),
    );
    let d = run(
        &mut conn,
        &ctx(FOCUS, "run-1", "open"),
        &o,
        &vars(&[("e", "ent-router"), ("l", "Router")]),
    )
    .await
    .unwrap();
    assert_eq!(d.outcome, Outcome::AlreadyApplied);
    assert_eq!(d.published.map(|p| p.1), Some(id));
    assert_eq!(count(&mut conn, "case_records").await, 1);

    // Without a store_target nothing is published.
    let silent = op(
        "case_open",
        None,
        Some(serde_json::json!({"entity": "e", "label": "l"})),
    );
    let d = run(
        &mut conn,
        &ctx(FOCUS, "run-2", "open"),
        &silent,
        &vars(&[("e", "ent-router"), ("l", "Router")]),
    )
    .await
    .unwrap();
    assert_eq!((d.outcome, d.published), (Outcome::Applied, None));
}

#[tokio::test]
async fn case_update_changes_only_the_fields_named_and_moves_the_version() {
    let _root = TestRoot::new().await;
    let mut conn = open(PERSONA).await;
    let id = open_a_case(&mut conn, FOCUS, "run-1").await;
    let o = op(
        "case_update",
        None,
        Some(serde_json::json!({"case": "c", "state": "st", "rung": "r", "outcome": "oc"})),
    );
    let d = run(
        &mut conn,
        &ctx(FOCUS, "run-2", "upd"),
        &o,
        &vars(&[
            ("c", &id),
            ("st", " in-progress "),
            ("r", "3"),
            ("oc", "restarted"),
        ]),
    )
    .await
    .unwrap();
    assert_eq!(d.outcome, Outcome::Applied);
    let case = case_store::get_case(&mut conn, &id).await.unwrap().unwrap();
    assert_eq!(case.state, CaseState::InProgress);
    assert_eq!(case.current_rung, 3);
    assert_eq!(case.outcome.as_deref(), Some("restarted"));
    assert_eq!(
        case.symptoms, "drops at night",
        "an unnamed field is untouched"
    );
    assert_eq!(case.version, 2);
}

#[tokio::test]
async fn strategy_append_records_the_cases_rung_and_a_replay_is_a_duplicate() {
    let _root = TestRoot::new().await;
    let mut conn = open(PERSONA).await;
    let id = open_a_case(&mut conn, FOCUS, "run-1").await;
    // Move the case to rung 2 first: rung_at is the rung the case stood at.
    let upd = op(
        "case_update",
        None,
        Some(serde_json::json!({"case": "c", "rung": "r"})),
    );
    run(
        &mut conn,
        &ctx(FOCUS, "run-1", "upd"),
        &upd,
        &vars(&[("c", &id), ("r", "2")]),
    )
    .await
    .unwrap();

    let o = op(
        "strategy_append",
        None,
        Some(serde_json::json!({
            "case": "c", "strategy": "s", "outcome": "oc", "user_signal": "sig"
        })),
    );
    let v = vars(&[
        ("c", &id),
        ("s", "isolate the network layer"),
        ("oc", "router answers ping"),
        ("sig", "already tried that"),
    ]);
    let d = run(&mut conn, &ctx(FOCUS, "run-1", "strat"), &o, &v)
        .await
        .unwrap();
    assert_eq!(d.outcome, Outcome::Applied);
    let rows = case_store::list_case_strategies(&mut conn, &id)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].rung_at, Some(2));
    assert_eq!(rows[0].source_run_id, "run-1");
    assert_eq!(rows[0].source_action_id, "strat");
    assert_eq!(rows[0].source_attempt, 0);
    assert_eq!(rows[0].user_signal.as_deref(), Some("already tried that"));

    let d = run(&mut conn, &ctx(FOCUS, "run-1", "strat"), &o, &v)
        .await
        .unwrap();
    assert_eq!(d.outcome, Outcome::AlreadyApplied);
    assert_eq!(count(&mut conn, "case_strategies").await, 1);

    // An empty optional slot is simply not recorded.
    let mut v2 = v.clone();
    v2.insert("sig".into(), "   ".into());
    run(&mut conn, &ctx(FOCUS, "run-2", "strat"), &o, &v2)
        .await
        .unwrap();
    let rows = case_store::list_case_strategies(&mut conn, &id)
        .await
        .unwrap();
    assert_eq!(rows[1].user_signal, None);
}

#[tokio::test]
async fn delta_record_carries_the_focus_and_the_source_case_and_publishes_its_id() {
    let _root = TestRoot::new().await;
    let mut conn = open(PERSONA).await;
    let case_id = open_a_case(&mut conn, FOCUS, "run-1").await;
    let o = op(
        "delta_record",
        Some("new_delta"),
        Some(serde_json::json!({
            "entities": "ents", "description": "d", "tags": "tg", "case": "c"
        })),
    );
    let v = vars(&[
        ("ents", r#"["ent-router", " ent-modem "]"#),
        ("d", "changed the DNS servers"),
        ("tg", r#"["dns"]"#),
        ("c", &case_id),
    ]);
    let d = run(&mut conn, &ctx(FOCUS, "run-2", "rec"), &o, &v)
        .await
        .unwrap();
    assert_eq!(d.outcome, Outcome::Applied);
    assert_eq!(
        d.published,
        Some(("new_delta".to_owned(), "run-2:rec".to_owned()))
    );

    let deltas =
        case_store::list_config_deltas_for_entity(&mut conn, "ent-router", Some(FOCUS), Utc::now())
            .await
            .unwrap();
    assert_eq!(deltas.len(), 1);
    assert_eq!(deltas[0].focus_id, FOCUS);
    assert_eq!(deltas[0].source_case_id.as_deref(), Some(case_id.as_str()));
    assert_eq!(deltas[0].relevance_tags, ["dns"]);
    assert_eq!(deltas[0].stored_status, DeltaStatus::BelievedActive);
    let modem =
        case_store::list_config_deltas_for_entity(&mut conn, "ent-modem", Some(FOCUS), Utc::now())
            .await
            .unwrap();
    assert_eq!(modem.len(), 1, "the second entity was trimmed and linked");

    let d = run(&mut conn, &ctx(FOCUS, "run-2", "rec"), &o, &v)
        .await
        .unwrap();
    assert_eq!(d.outcome, Outcome::AlreadyApplied);
    assert_eq!(count(&mut conn, "config_deltas").await, 1);

    // The optional slots may be left out.
    let bare = op(
        "delta_record",
        None,
        Some(serde_json::json!({"entities": "ents", "description": "d"})),
    );
    run(&mut conn, &ctx(FOCUS, "run-3", "rec"), &bare, &v)
        .await
        .unwrap();
    assert_eq!(count(&mut conn, "config_deltas").await, 2);
}

#[tokio::test]
async fn delta_confirm_and_delta_changed_set_the_two_statuses_with_the_clock_at_now() {
    let _root = TestRoot::new().await;
    let mut conn = open(PERSONA).await;
    let rec = op(
        "delta_record",
        None,
        Some(serde_json::json!({"entities": "ents", "description": "d"})),
    );
    let before = Utc::now();
    run(
        &mut conn,
        &ctx(FOCUS, "run-1", "rec"),
        &rec,
        &vars(&[("ents", r#"["ent-router"]"#), ("d", "changed DNS")]),
    )
    .await
    .unwrap();
    let ids = vars(&[
        ("d", "run-1:rec"),
        ("e", "ent-router"),
        ("h", &handles::delta_handle(FOCUS, "run-1:rec")),
    ]);

    // items.id=889: confirmation names the delta by its HANDLE.
    let confirm = op(
        "delta_confirm",
        None,
        Some(serde_json::json!({"handle": "h"})),
    );
    let d = run(&mut conn, &ctx(FOCUS, "run-2", "conf"), &confirm, &ids)
        .await
        .unwrap();
    assert_eq!(d.outcome, Outcome::Applied);
    let row = &case_store::list_config_deltas_for_entity(
        &mut conn,
        "ent-router",
        Some(FOCUS),
        Utc::now(),
    )
    .await
    .unwrap()[0];
    assert_eq!(row.stored_status, DeltaStatus::ConfirmedActive);
    // at = None: the clock is the moment of the write, not a value a row chose.
    let stamped = chrono::DateTime::parse_from_rfc3339(&row.status_changed_at).unwrap();
    assert!(stamped >= before - chrono::Duration::seconds(1));

    let changed = op(
        "delta_changed",
        None,
        Some(serde_json::json!({"delta": "d", "entity": "e"})),
    );
    run(&mut conn, &ctx(FOCUS, "run-3", "evt"), &changed, &ids)
        .await
        .unwrap();
    let row = &case_store::list_config_deltas_for_entity(
        &mut conn,
        "ent-router",
        Some(FOCUS),
        Utc::now(),
    )
    .await
    .unwrap()[0];
    assert_eq!(row.stored_status, DeltaStatus::BelievedChanged);
}

// ---------------------------------------------------------------------------
// The one-time latch
// ---------------------------------------------------------------------------

/// Every writer except the latch itself, with arguments that resolve.
fn every_writer() -> Vec<(StoreOp, HashMap<String, String>)> {
    let v = vars(&[
        ("val", "billing"),
        ("e", "ent-router"),
        ("l", "Router"),
        ("c", "run-0:open"),
        ("st", "in-progress"),
        ("s", "a strategy"),
        ("oc", "an outcome"),
        ("ents", r#"["ent-router"]"#),
        ("d", "a change"),
        ("did", "run-0:rec"),
        ("th", &handles::case_handle(FOCUS, "run-0:open")),
        ("dh", &handles::delta_handle(FOCUS, "run-0:rec")),
    ]);
    vec![
        (
            op(
                "session_put",
                Some("locked_intent"),
                Some(serde_json::json!({"value": "val"})),
            ),
            v.clone(),
        ),
        (
            op(
                "case_open",
                Some("nc"),
                Some(serde_json::json!({"entity": "e", "label": "l"})),
            ),
            v.clone(),
        ),
        (
            op(
                "case_update",
                None,
                Some(serde_json::json!({"case": "c", "state": "st"})),
            ),
            v.clone(),
        ),
        (
            op(
                "strategy_append",
                None,
                Some(serde_json::json!({"case": "c", "strategy": "s", "outcome": "oc"})),
            ),
            v.clone(),
        ),
        (
            op(
                "delta_record",
                None,
                Some(serde_json::json!({"entities": "ents", "description": "d"})),
            ),
            v.clone(),
        ),
        (
            op(
                "delta_confirm",
                None,
                Some(serde_json::json!({"handle": "dh"})),
            ),
            v.clone(),
        ),
        (
            op(
                "delta_changed",
                None,
                Some(serde_json::json!({"delta": "did", "entity": "e"})),
            ),
            v.clone(),
        ),
        // items.id=889.
        (
            op(
                "case_select",
                None,
                Some(serde_json::json!({"handle": "th"})),
            ),
            v.clone(),
        ),
        (
            op(
                "case_link",
                None,
                Some(serde_json::json!({"case": "c", "entity": "e"})),
            ),
            v.clone(),
        ),
        (
            op(
                "entity_changed",
                None,
                Some(serde_json::json!({"case": "c"})),
            ),
            v.clone(),
        ),
        (op("session_clear", None, None), v),
    ]
}

async fn row_counts(conn: &mut SqliteConnection) -> Vec<i64> {
    let mut out = Vec::new();
    for t in [
        "session_variables",
        "case_records",
        "case_entities",
        "case_strategies",
        "config_deltas",
        "config_delta_entities",
    ] {
        out.push(count(conn, t).await);
    }
    out
}

#[tokio::test]
async fn a_latched_chat_refuses_every_writer_and_writes_nothing() {
    let _root = TestRoot::new().await;
    let mut conn = open(PERSONA).await;
    // The open chat has a case and a delta, so the writers below have real data
    // to touch if the latch failed to hold.
    open_a_case(&mut conn, FOCUS, "run-0").await;
    let rec = op(
        "delta_record",
        None,
        Some(serde_json::json!({"entities": "ents", "description": "d"})),
    );
    run(
        &mut conn,
        &ctx(FOCUS, "run-0", "rec"),
        &rec,
        &vars(&[("ents", r#"["ent-router"]"#), ("d", "x")]),
    )
    .await
    .unwrap();

    run(
        &mut conn,
        &ctx(FOCUS, "run-1", "latch"),
        &op("latch", None, None),
        &vars(&[]),
    )
    .await
    .unwrap();
    let before = row_counts(&mut conn).await;
    let case_before = case_store::get_case(&mut conn, "run-0:open")
        .await
        .unwrap()
        .unwrap();

    for (writer, v) in every_writer() {
        let d = run(&mut conn, &ctx(FOCUS, "run-2", "w"), &writer, &v)
            .await
            .unwrap_or_else(|e| {
                panic!(
                    "{}: a latch refusal is not an error: {e}",
                    writer.op.as_str()
                )
            });
        assert_eq!(d.outcome, Outcome::RefusedOneTime, "{}", writer.op.as_str());
        assert_eq!(d.published, None, "{}", writer.op.as_str());
    }
    assert_eq!(row_counts(&mut conn).await, before, "nothing was written");
    let case_after = case_store::get_case(&mut conn, "run-0:open")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(case_after, case_before, "the case is untouched");
    let deltas =
        case_store::list_config_deltas_for_entity(&mut conn, "ent-router", Some(FOCUS), Utc::now())
            .await
            .unwrap();
    assert_eq!(deltas[0].stored_status, DeltaStatus::BelievedActive);
}

/// The latch is chat-wide: another Focus's writer in the same chat is refused.
#[tokio::test]
async fn the_latch_covers_the_chat_not_one_focus() {
    let _root = TestRoot::new().await;
    let mut conn = open(PERSONA).await;
    run(
        &mut conn,
        &ctx(FOCUS, "run-1", "latch"),
        &op("latch", None, None),
        &vars(&[]),
    )
    .await
    .unwrap();
    let put = op(
        "session_put",
        Some("locked_intent"),
        Some(serde_json::json!({"value": "v"})),
    );
    let d = run(
        &mut conn,
        &ctx(OTHER_FOCUS, "run-2", "put"),
        &put,
        &vars(&[("v", "billing")]),
    )
    .await
    .unwrap();
    assert_eq!(d.outcome, Outcome::RefusedOneTime);
    assert_eq!(
        count(&mut conn, "session_variables").await,
        1,
        "only the latch row"
    );
}

/// In a latched chat EVERY writer is a refusal, even one whose arguments would
/// not resolve (an id a refused case_open never published, say).
#[tokio::test]
async fn a_latched_chat_refuses_a_row_whose_arguments_do_not_resolve() {
    let _root = TestRoot::new().await;
    let mut conn = open(PERSONA).await;
    run(
        &mut conn,
        &ctx(FOCUS, "run-1", "latch"),
        &op("latch", None, None),
        &vars(&[]),
    )
    .await
    .unwrap();
    let upd = op(
        "case_update",
        None,
        Some(serde_json::json!({"case": "never_published", "state": "st"})),
    );
    let d = run(&mut conn, &ctx(FOCUS, "run-2", "u"), &upd, &vars(&[]))
        .await
        .unwrap();
    assert_eq!(d.outcome, Outcome::RefusedOneTime);
}

/// The store, not the handler, holds the latch: a latch that is set BETWEEN the
/// handler's checks and its write still refuses the write. Here the latch row is
/// set directly through the store while a write is prepared, then the handler
/// runs: the store's own in-transaction check is what answers.
#[tokio::test]
async fn the_store_layer_gate_refuses_even_when_the_handler_expects_an_open_chat() {
    let _root = TestRoot::new().await;
    let mut conn = open(PERSONA).await;
    // A second connection to the same file sets the latch.
    let mut other = open(PERSONA).await;
    session_store::latch_write_suppressed(&mut other, CHAT, FOCUS, "run-x")
        .await
        .unwrap();
    // The handler never saw write_mode: it goes straight to the typed write.
    let put = op(
        "session_put",
        Some("locked_intent"),
        Some(serde_json::json!({"value": "v"})),
    );
    let d = run(
        &mut conn,
        &ctx(FOCUS, "run-1", "put"),
        &put,
        &vars(&[("v", "billing")]),
    )
    .await
    .unwrap();
    assert_eq!(d.outcome, Outcome::RefusedOneTime);
    assert!(
        session_store::get_session_variable(&mut conn, CHAT, FOCUS, "locked_intent")
            .await
            .unwrap()
            .is_none()
    );
}

/// A mis-authored row that LOAD would never let through (session_put aimed at
/// write_mode) still cannot touch the latch: the store reserves the name.
#[tokio::test]
async fn a_mis_authored_row_aimed_at_the_latch_is_refused_by_the_store() {
    let _root = TestRoot::new().await;
    let mut conn = open(PERSONA).await;
    let hand_built = StoreOp {
        op: crate::conductor::store_op::Op::SessionPut,
        target: Some("write_mode".to_owned()),
        args: vec![(crate::conductor::store_op::Slot::Value, "v".to_owned())],
    };
    let err = run(
        &mut conn,
        &ctx(FOCUS, "run-1", "put"),
        &hand_built,
        &vars(&[("v", "suppressed")]),
    )
    .await
    .unwrap_err();
    assert!(
        matches!(
            err,
            StoreWriteError::Session(SessionStoreError::ReservedName(_))
        ),
        "{err}"
    );
    assert_eq!(
        session_store::write_mode(&mut conn, CHAT).await.unwrap(),
        WriteMode::Open,
        "the chat was not latched by a session_put"
    );
    assert_eq!(count(&mut conn, "session_variables").await, 0);
}

#[tokio::test]
async fn an_unreadable_latch_fails_the_writer_closed_instead_of_writing() {
    let _root = TestRoot::new().await;
    let mut conn = open(PERSONA).await;
    sqlx::query("DROP TABLE session_variables")
        .execute(&mut conn)
        .await
        .unwrap();
    let put = op(
        "session_put",
        Some("locked_intent"),
        Some(serde_json::json!({"value": "v"})),
    );
    let err = run(
        &mut conn,
        &ctx(FOCUS, "run-1", "put"),
        &put,
        &vars(&[("v", "billing")]),
    )
    .await
    .unwrap_err();
    assert!(!matches!(
        err,
        StoreWriteError::Session(SessionStoreError::Latch(LatchError::Suppressed))
    ));
}

// ---------------------------------------------------------------------------
// focus_id scoping (decisions.id=509) and persona isolation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn another_focus_cannot_write_a_case_or_a_delta_it_did_not_record() {
    let _root = TestRoot::new().await;
    let mut conn = open(PERSONA).await;
    let case_id = open_a_case(&mut conn, FOCUS, "run-1").await;
    let rec = op(
        "delta_record",
        Some("nd"),
        Some(serde_json::json!({"entities": "ents", "description": "d"})),
    );
    run(
        &mut conn,
        &ctx(FOCUS, "run-1", "rec"),
        &rec,
        &vars(&[("ents", r#"["ent-router"]"#), ("d", "changed DNS")]),
    )
    .await
    .unwrap();
    let case_before = case_store::get_case(&mut conn, &case_id)
        .await
        .unwrap()
        .unwrap();

    let foreign = ctx(OTHER_FOCUS, "run-9", "x");
    let v = vars(&[
        ("c", &case_id),
        ("st", "escalated"),
        ("s", "s"),
        ("oc", "o"),
        ("d", "run-1:rec"),
        ("e", "ent-router"),
    ]);
    for (name, args) in [
        (
            "case_update",
            serde_json::json!({"case": "c", "state": "st"}),
        ),
        (
            "strategy_append",
            serde_json::json!({"case": "c", "strategy": "s", "outcome": "oc"}),
        ),
    ] {
        let err = run(&mut conn, &foreign, &op(name, None, Some(args)), &v)
            .await
            .unwrap_err();
        assert!(
            matches!(err, StoreWriteError::CaseNotInFocus),
            "{name}: {err}"
        );
    }
    let err = run(
        &mut conn,
        &foreign,
        &op(
            "delta_changed",
            None,
            Some(serde_json::json!({"delta": "d", "entity": "e"})),
        ),
        &v,
    )
    .await
    .unwrap_err();
    assert!(
        matches!(err, StoreWriteError::DeltaNotInFocus),
        "delta_changed: {err}"
    );
    // delta_confirm names the delta by handle: the first Focus's handle is no
    // handle at all for another Focus (the Focus is inside the hash and the
    // lookup is scoped by it).
    let mut hv = v.clone();
    hv.insert("h".into(), handles::delta_handle(FOCUS, "run-1:rec"));
    let err = run(
        &mut conn,
        &foreign,
        &op(
            "delta_confirm",
            None,
            Some(serde_json::json!({"handle": "h"})),
        ),
        &hv,
    )
    .await
    .unwrap_err();
    assert!(
        matches!(
            err,
            StoreWriteError::Handle(crate::persistence::handles::HandleError::NotFound(_))
        ),
        "delta_confirm: {err}"
    );
    // A delta may not claim another Focus's case as its source.
    let claim = op(
        "delta_record",
        None,
        Some(serde_json::json!({"entities": "ents", "description": "d", "case": "c"})),
    );
    let mut v2 = v.clone();
    v2.insert("ents".into(), r#"["ent-router"]"#.into());
    run(&mut conn, &foreign, &claim, &v2).await.unwrap_err();

    // Nothing of the first Focus changed.
    assert_eq!(
        case_store::get_case(&mut conn, &case_id)
            .await
            .unwrap()
            .unwrap(),
        case_before
    );
    assert!(case_store::list_case_strategies(&mut conn, &case_id)
        .await
        .unwrap()
        .is_empty());
    let own =
        case_store::list_config_deltas_for_entity(&mut conn, "ent-router", Some(FOCUS), Utc::now())
            .await
            .unwrap();
    assert_eq!(own[0].stored_status, DeltaStatus::BelievedActive);
    assert_eq!(count(&mut conn, "config_deltas").await, 1);
}

/// A case or delta a Focus opens carries ITS focus_id, whatever the rows say, and
/// the Focus's own session variables are separate rows.
#[tokio::test]
async fn writes_are_stamped_with_the_running_focus_and_session_rows_do_not_cross() {
    let _root = TestRoot::new().await;
    let mut conn = open(PERSONA).await;
    let a = open_a_case(&mut conn, FOCUS, "run-1").await;
    let b = open_a_case(&mut conn, OTHER_FOCUS, "run-2").await;
    assert_eq!(
        case_store::get_case(&mut conn, &a)
            .await
            .unwrap()
            .unwrap()
            .focus_id,
        FOCUS
    );
    assert_eq!(
        case_store::get_case(&mut conn, &b)
            .await
            .unwrap()
            .unwrap()
            .focus_id,
        OTHER_FOCUS
    );
    assert_eq!(
        case_store::list_open_cases(&mut conn, FOCUS)
            .await
            .unwrap()
            .len(),
        1
    );

    let put = op(
        "session_put",
        Some("locked_intent"),
        Some(serde_json::json!({"value": "v"})),
    );
    run(
        &mut conn,
        &ctx(FOCUS, "run-1", "put"),
        &put,
        &vars(&[("v", "billing")]),
    )
    .await
    .unwrap();
    run(
        &mut conn,
        &ctx(OTHER_FOCUS, "run-2", "put"),
        &put,
        &vars(&[("v", "network")]),
    )
    .await
    .unwrap();
    for (focus, want) in [(FOCUS, "billing"), (OTHER_FOCUS, "network")] {
        let row = session_store::get_session_variable(&mut conn, CHAT, focus, "locked_intent")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.value, want, "{focus}");
        assert_eq!(row.version, 1, "{focus}: each Focus has its own row");
    }
}

#[tokio::test]
async fn one_personas_writes_are_invisible_in_another_personas_file() {
    let _root = TestRoot::new().await;
    let mut one = open("persona-one").await;
    let mut two = open("persona-two").await;
    let id = open_a_case(&mut one, FOCUS, "run-1").await;
    let put = op(
        "session_put",
        Some("active_case_id"),
        Some(serde_json::json!({"value": "c"})),
    );
    run(
        &mut one,
        &ctx(FOCUS, "run-1", "put"),
        &put,
        &vars(&[("c", &id)]),
    )
    .await
    .unwrap();
    run(
        &mut one,
        &ctx(FOCUS, "run-1", "latch"),
        &op("latch", None, None),
        &vars(&[]),
    )
    .await
    .unwrap();

    assert_eq!(row_counts(&mut two).await, [0, 0, 0, 0, 0, 0]);
    assert!(case_store::get_case(&mut two, &id).await.unwrap().is_none());
    assert_eq!(
        session_store::write_mode(&mut two, CHAT).await.unwrap(),
        WriteMode::Open,
        "persona one's latch does not close persona two's chat"
    );
    // And the writer works in persona two, unaffected, for a case of its own.
    let id_two = open_a_case(&mut two, FOCUS, "run-2").await;
    let d = run(
        &mut two,
        &ctx(FOCUS, "run-2", "put"),
        &put,
        &vars(&[("c", &id_two)]),
    )
    .await
    .unwrap();
    assert_eq!(d.outcome, Outcome::Applied);
    let row = session_store::get_session_variable(&mut one, CHAT, FOCUS, "active_case_id")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.value, id);
}

/// The chat's active case must be a case this Focus opened.
#[tokio::test]
async fn active_case_id_can_only_name_a_case_of_the_running_focus() {
    let _root = TestRoot::new().await;
    let mut conn = open(PERSONA).await;
    let mine = open_a_case(&mut conn, FOCUS, "run-1").await;
    let theirs = open_a_case(&mut conn, OTHER_FOCUS, "run-2").await;
    let put = op(
        "session_put",
        Some("active_case_id"),
        Some(serde_json::json!({"value": "c"})),
    );
    let d = run(
        &mut conn,
        &ctx(FOCUS, "run-3", "put"),
        &put,
        &vars(&[("c", &mine)]),
    )
    .await
    .unwrap();
    assert_eq!(d.outcome, Outcome::Applied);
    for bad in [theirs.as_str(), "no-such-case"] {
        let err = run(
            &mut conn,
            &ctx(FOCUS, "run-4", "put"),
            &put,
            &vars(&[("c", bad)]),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(err, StoreWriteError::CaseNotInFocus),
            "{bad}: {err}"
        );
    }
    let row = session_store::get_session_variable(&mut conn, CHAT, FOCUS, "active_case_id")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.value, mine);
}

// ---------------------------------------------------------------------------
// A handler failure is an error (the pass fails closed)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn bad_arguments_and_store_errors_are_errors_not_silent_skips() {
    let _root = TestRoot::new().await;
    let mut conn = open(PERSONA).await;
    let id = open_a_case(&mut conn, FOCUS, "run-1").await;
    let c = ctx(FOCUS, "run-2", "w");

    // No chat at all.
    let put = op(
        "session_put",
        Some("locked_intent"),
        Some(serde_json::json!({"value": "v"})),
    );
    let no_chat = WriterContext {
        context_key: None,
        ..c
    };
    assert!(matches!(
        run(&mut conn, &no_chat, &put, &vars(&[("v", "billing")]))
            .await
            .unwrap_err(),
        StoreWriteError::NoContextKey
    ));
    // A variable that is not set; an empty required value.
    assert!(matches!(
        run(&mut conn, &c, &put, &vars(&[])).await.unwrap_err(),
        StoreWriteError::ArgumentUnset { .. }
    ));
    assert!(matches!(
        run(&mut conn, &c, &put, &vars(&[("v", "   ")]))
            .await
            .unwrap_err(),
        StoreWriteError::ArgumentEmpty { .. }
    ));
    // A value the session name's own spec rejects (an intent is an identifier).
    assert!(matches!(
        run(&mut conn, &c, &put, &vars(&[("v", "Not A Valid Intent!")]))
            .await
            .unwrap_err(),
        StoreWriteError::InvalidSessionValue(_)
    ));
    // The model said something that is not a state, a rung, a path.
    for (slot, bad) in [
        ("state", "finished"),
        ("rung", "9"),
        ("rung", "two"),
        ("resolution_path", "gave up"),
    ] {
        let o = op(
            "case_update",
            None,
            Some(serde_json::json!({"case": "c", slot: "x"})),
        );
        let err = run(&mut conn, &c, &o, &vars(&[("c", &id), ("x", bad)]))
            .await
            .unwrap_err();
        assert!(
            matches!(err, StoreWriteError::ArgumentInvalid { .. }),
            "{slot}={bad}: {err}"
        );
    }
    // A list that is not a JSON array, an empty item, too many items.
    let rec = op(
        "delta_record",
        None,
        Some(serde_json::json!({"entities": "ents", "description": "d"})),
    );
    for bad in [
        "ent-router".to_owned(),
        r#"["a", " "]"#.to_owned(),
        serde_json::to_string(
            &(0..=MAX_LIST_ITEMS)
                .map(|i| format!("e{i}"))
                .collect::<Vec<_>>(),
        )
        .unwrap(),
    ] {
        let err = run(&mut conn, &c, &rec, &vars(&[("ents", &bad), ("d", "x")]))
            .await
            .unwrap_err();
        assert!(
            matches!(err, StoreWriteError::ArgumentInvalid { .. }),
            "{bad}: {err}"
        );
    }
    assert_eq!(count(&mut conn, "config_deltas").await, 0);
    // The error text names the slot, never the model's value.
    let err = run(&mut conn, &c, &put, &vars(&[("v", "Secret Value!")]))
        .await
        .unwrap_err();
    assert!(!err.to_string().contains("Secret"), "{err}");
}

#[tokio::test]
async fn a_closed_case_takes_no_writer_and_the_pass_is_told() {
    let _root = TestRoot::new().await;
    let mut conn = open(PERSONA).await;
    let id = open_a_case(&mut conn, FOCUS, "run-1").await;
    let close = op(
        "case_update",
        None,
        Some(serde_json::json!({"case": "c", "state": "st"})),
    );
    run(
        &mut conn,
        &ctx(FOCUS, "run-1", "close"),
        &close,
        &vars(&[("c", &id), ("st", "resolved")]),
    )
    .await
    .unwrap();
    let strat = op(
        "strategy_append",
        None,
        Some(serde_json::json!({"case": "c", "strategy": "s", "outcome": "oc"})),
    );
    let err = run(
        &mut conn,
        &ctx(FOCUS, "run-2", "strat"),
        &strat,
        &vars(&[("c", &id), ("s", "x"), ("oc", "y")]),
    )
    .await
    .unwrap_err();
    assert!(
        matches!(
            err,
            StoreWriteError::Case(crate::persistence::case_store::CaseStoreError::CaseClosed(
                _
            ))
        ),
        "{err}"
    );
    assert!(case_store::list_case_strategies(&mut conn, &id)
        .await
        .unwrap()
        .is_empty());
    let links = case_store::list_case_entities(&mut conn, &id)
        .await
        .unwrap();
    assert_eq!(links.len(), 1);
    assert_eq!(links[0].role, EntityRole::Primary);
}

// ---------------------------------------------------------------------------
// items.id=889: handles, case_select, case_link, entity_changed, delta_confirm
// by handle, session_clear
// ---------------------------------------------------------------------------

use crate::persistence::handles::HandleError;

/// delta_record then return the delta id it published.
async fn record_a_delta(
    conn: &mut SqliteConnection,
    focus: &str,
    run_id: &str,
    entities: &str,
) -> String {
    let o = op(
        "delta_record",
        Some("new_delta"),
        Some(serde_json::json!({"entities": "ents", "description": "d"})),
    );
    let d = run(
        conn,
        &ctx(focus, run_id, "rec"),
        &o,
        &vars(&[("ents", entities), ("d", "changed DNS")]),
    )
    .await
    .expect("delta_record");
    assert_eq!(d.outcome, Outcome::Applied);
    d.published.expect("published the delta id").1
}

fn select_op() -> StoreOp {
    op(
        "case_select",
        None,
        Some(serde_json::json!({"handle": "h"})),
    )
}

async fn active_case(conn: &mut SqliteConnection, focus: &str) -> Option<String> {
    session_store::get_session_variable(conn, CHAT, focus, "active_case_id")
        .await
        .unwrap()
        .map(|v| v.value)
}

#[tokio::test]
async fn case_select_resolves_a_ticket_handle_store_side_to_the_active_case() {
    let _root = TestRoot::new().await;
    let mut conn = open(PERSONA).await;
    let one = open_a_case(&mut conn, FOCUS, "run-1").await;
    let two = open_a_case(&mut conn, FOCUS, "run-2").await;
    assert_ne!(one, two);

    let h1 = handles::case_handle(FOCUS, &one);
    let d = run(
        &mut conn,
        &ctx(FOCUS, "run-3", "sel"),
        &select_op(),
        &vars(&[("h", &h1)]),
    )
    .await
    .unwrap();
    assert_eq!(d.outcome, Outcome::Applied);
    assert_eq!(
        active_case(&mut conn, FOCUS).await.as_deref(),
        Some(one.as_str())
    );

    // The same handle again changes nothing.
    let d = run(
        &mut conn,
        &ctx(FOCUS, "run-4", "sel"),
        &select_op(),
        &vars(&[("h", &h1)]),
    )
    .await
    .unwrap();
    assert_eq!(d.outcome, Outcome::AlreadyApplied);

    // Another ticket, the way a model echoes a token: lower case, in brackets.
    let h2 = format!("[{}]", handles::case_handle(FOCUS, &two).to_lowercase());
    let d = run(
        &mut conn,
        &ctx(FOCUS, "run-5", "sel"),
        &select_op(),
        &vars(&[("h", &h2)]),
    )
    .await
    .unwrap();
    assert_eq!(d.outcome, Outcome::Applied);
    assert_eq!(
        active_case(&mut conn, FOCUS).await.as_deref(),
        Some(two.as_str())
    );
    let row = session_store::get_session_variable(&mut conn, CHAT, FOCUS, "active_case_id")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.version, 2, "moved through the compare-and-set");
}

#[tokio::test]
async fn case_select_with_a_wrong_or_foreign_handle_writes_nothing_and_fails_closed() {
    let _root = TestRoot::new().await;
    let mut conn = open(PERSONA).await;
    let mine = open_a_case(&mut conn, FOCUS, "run-1").await;
    // A case another Focus opened, with its own handle.
    let theirs = open_a_case(&mut conn, OTHER_FOCUS, "run-9").await;
    let delta = record_a_delta(&mut conn, FOCUS, "run-2", r#"["ent-router"]"#).await;

    // Closing a case takes it out of reach.
    let closed = open_a_case(&mut conn, FOCUS, "run-3").await;
    let c = case_store::get_case(&mut conn, &closed)
        .await
        .unwrap()
        .unwrap();
    case_store::update_case(
        &mut conn,
        CHAT,
        &closed,
        c.version,
        &case_store::CaseUpdate {
            state: Some(CaseState::Resolved),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let before = row_counts(&mut conn).await;
    let cases: Vec<(&str, String)> = vec![
        ("empty", String::new()),
        ("garbage", "banana".to_owned()),
        ("an internal id", mine.clone()),
        ("wrong length", "T-ABC".to_owned()),
        ("right shape, no such ticket", "T-00000000".to_owned()),
        ("a delta handle", handles::delta_handle(FOCUS, &delta)),
        // Another Focus's ticket, under either Focus's hash.
        (
            "foreign, own hash",
            handles::case_handle(OTHER_FOCUS, &theirs),
        ),
        ("foreign, our hash", handles::case_handle(FOCUS, &theirs)),
        ("closed ticket", handles::case_handle(FOCUS, &closed)),
    ];
    for (label, handle) in cases {
        let err = run(
            &mut conn,
            &ctx(FOCUS, "run-5", "sel"),
            &select_op(),
            &vars(&[("h", &handle)]),
        )
        .await
        .unwrap_err();
        match &err {
            StoreWriteError::Handle(HandleError::Malformed(_))
            | StoreWriteError::Handle(HandleError::NotFound(_)) => {}
            StoreWriteError::ArgumentEmpty { .. } if label == "empty" => {}
            other => panic!("{label}: {other}"),
        }
        assert!(
            !err.to_string().contains(&handle) || handle.is_empty(),
            "{label}: the error must not echo the value: {err}"
        );
    }
    assert_eq!(row_counts(&mut conn).await, before, "nothing was written");
    assert_eq!(active_case(&mut conn, FOCUS).await, None);

    // The foreign Focus can still select its own case, and only its own.
    let d = run(
        &mut conn,
        &ctx(OTHER_FOCUS, "run-9", "sel"),
        &select_op(),
        &vars(&[("h", &handles::case_handle(OTHER_FOCUS, &theirs))]),
    )
    .await
    .unwrap();
    assert_eq!(d.outcome, Outcome::Applied);
    assert_eq!(
        active_case(&mut conn, OTHER_FOCUS).await.as_deref(),
        Some(theirs.as_str())
    );
    assert_eq!(
        active_case(&mut conn, FOCUS).await,
        None,
        "session variables are per Focus"
    );
}

#[tokio::test]
async fn case_link_adds_an_affected_entity_and_a_closed_case_takes_no_link() {
    let _root = TestRoot::new().await;
    let mut conn = open(PERSONA).await;
    let id = open_a_case(&mut conn, FOCUS, "run-1").await;
    let link = op(
        "case_link",
        None,
        Some(serde_json::json!({"case": "c", "entity": "e"})),
    );
    let v = vars(&[("c", &id), ("e", "ent-modem")]);
    let d = run(&mut conn, &ctx(FOCUS, "run-2", "link"), &link, &v)
        .await
        .unwrap();
    assert_eq!(d.outcome, Outcome::Applied);
    let links = case_store::list_case_entities(&mut conn, &id)
        .await
        .unwrap();
    assert_eq!(links.len(), 2);
    assert!(links
        .iter()
        .any(|l| l.entity_id == "ent-modem" && l.role == EntityRole::Affected));
    let d = run(&mut conn, &ctx(FOCUS, "run-3", "link"), &link, &v)
        .await
        .unwrap();
    assert_eq!(d.outcome, Outcome::AlreadyApplied);
    assert_eq!(count(&mut conn, "case_entities").await, 2);

    // Another Focus cannot link to this Focus's case.
    let err = run(&mut conn, &ctx(OTHER_FOCUS, "run-9", "link"), &link, &v)
        .await
        .unwrap_err();
    assert!(matches!(err, StoreWriteError::CaseNotInFocus), "{err}");

    // A closed case takes no further links.
    let case = case_store::get_case(&mut conn, &id).await.unwrap().unwrap();
    case_store::update_case(
        &mut conn,
        CHAT,
        &id,
        case.version,
        &case_store::CaseUpdate {
            state: Some(CaseState::Closed),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let err = run(
        &mut conn,
        &ctx(FOCUS, "run-4", "link"),
        &link,
        &vars(&[("c", &id), ("e", "ent-printer")]),
    )
    .await
    .unwrap_err();
    assert!(
        matches!(err, StoreWriteError::Case(CaseStoreError::CaseClosed(_))),
        "{err}"
    );
    assert_eq!(count(&mut conn, "case_entities").await, 2);
}

#[tokio::test]
async fn entity_changed_moves_every_delta_of_the_cases_entity_for_this_focus_only() {
    let _root = TestRoot::new().await;
    let mut conn = open(PERSONA).await;
    let case = open_a_case(&mut conn, FOCUS, "run-1").await; // primary ent-router
    let d1 = record_a_delta(&mut conn, FOCUS, "run-2", r#"["ent-router"]"#).await;
    let d2 = record_a_delta(&mut conn, FOCUS, "run-3", r#"["ent-router","ent-modem"]"#).await;
    let other_entity = record_a_delta(&mut conn, FOCUS, "run-4", r#"["ent-printer"]"#).await;
    let other_focus = record_a_delta(&mut conn, OTHER_FOCUS, "run-5", r#"["ent-router"]"#).await;
    // One delta is already confirmed: the event still moves it.
    case_store::set_delta_entity_status(
        &mut conn,
        CHAT,
        &d1,
        "ent-router",
        DeltaStatus::ConfirmedActive,
        None,
    )
    .await
    .unwrap();

    let ev = op(
        "entity_changed",
        None,
        Some(serde_json::json!({"case": "c"})),
    );
    let v = vars(&[("c", &case)]);
    let before = Utc::now();
    let d = run(&mut conn, &ctx(FOCUS, "run-6", "evt"), &ev, &v)
        .await
        .unwrap();
    assert_eq!(d.outcome, Outcome::Applied);

    let status = |deltas: &[case_store::EntityConfigDelta], id: &str, entity: &str| {
        deltas
            .iter()
            .find(|d| d.delta_id == id && d.entity_id == entity)
            .map(|d| (d.stored_status, d.status_changed_at.clone()))
            .unwrap_or_else(|| panic!("{id}/{entity}"))
    };
    let router =
        case_store::list_config_deltas_for_entity(&mut conn, "ent-router", None, Utc::now())
            .await
            .unwrap();
    // Both of this Focus's router deltas moved, whatever they were.
    for id in [&d1, &d2] {
        let (st, at) = status(&router, id, "ent-router");
        assert_eq!(st, DeltaStatus::BelievedChanged, "{id}");
        // Never an explicit time: the clock is the moment of the write.
        let stamped = chrono::DateTime::parse_from_rfc3339(&at).unwrap();
        assert!(stamped >= before - chrono::Duration::seconds(1), "{id}");
    }
    // The other Focus's delta on the SAME entity is untouched (decisions.id=509).
    assert_eq!(
        status(&router, &other_focus, "ent-router").0,
        DeltaStatus::BelievedActive
    );
    // A delta's other entity is not the event's entity.
    let modem = case_store::list_config_deltas_for_entity(&mut conn, "ent-modem", None, Utc::now())
        .await
        .unwrap();
    assert_eq!(
        status(&modem, &d2, "ent-modem").0,
        DeltaStatus::BelievedActive
    );
    // A delta on another entity is untouched.
    let printer =
        case_store::list_config_deltas_for_entity(&mut conn, "ent-printer", None, Utc::now())
            .await
            .unwrap();
    assert_eq!(
        status(&printer, &other_entity, "ent-printer").0,
        DeltaStatus::BelievedActive
    );

    // The same event again has nothing to move.
    let d = run(&mut conn, &ctx(FOCUS, "run-7", "evt"), &ev, &v)
        .await
        .unwrap();
    assert_eq!(d.outcome, Outcome::AlreadyApplied);

    // The case must be this Focus's.
    let err = run(&mut conn, &ctx(OTHER_FOCUS, "run-9", "evt"), &ev, &v)
        .await
        .unwrap_err();
    assert!(matches!(err, StoreWriteError::CaseNotInFocus), "{err}");
}

#[tokio::test]
async fn delta_confirm_by_handle_confirms_every_entity_of_the_delta_for_this_focus() {
    let _root = TestRoot::new().await;
    let mut conn = open(PERSONA).await;
    let two_entities =
        record_a_delta(&mut conn, FOCUS, "run-1", r#"["ent-router","ent-modem"]"#).await;
    let foreign = record_a_delta(&mut conn, OTHER_FOCUS, "run-2", r#"["ent-router"]"#).await;
    let confirm = op(
        "delta_confirm",
        None,
        Some(serde_json::json!({"handle": "h"})),
    );

    let d = run(
        &mut conn,
        &ctx(FOCUS, "run-3", "conf"),
        &confirm,
        &vars(&[("h", &handles::delta_handle(FOCUS, &two_entities))]),
    )
    .await
    .unwrap();
    assert_eq!(d.outcome, Outcome::Applied);
    for entity in ["ent-router", "ent-modem"] {
        let rows =
            case_store::list_config_deltas_for_entity(&mut conn, entity, Some(FOCUS), Utc::now())
                .await
                .unwrap();
        assert_eq!(
            rows[0].stored_status,
            DeltaStatus::ConfirmedActive,
            "{entity}"
        );
    }

    // Unknown, malformed, a ticket handle and another Focus's delta: nothing.
    let before_foreign =
        case_store::list_config_deltas_for_entity(&mut conn, "ent-router", None, Utc::now())
            .await
            .unwrap();
    for handle in [
        "D-00000000".to_owned(),
        "not a handle".to_owned(),
        handles::case_handle(FOCUS, &two_entities),
        handles::delta_handle(OTHER_FOCUS, &foreign),
        handles::delta_handle(FOCUS, &foreign),
    ] {
        let err = run(
            &mut conn,
            &ctx(FOCUS, "run-4", "conf"),
            &confirm,
            &vars(&[("h", &handle)]),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, StoreWriteError::Handle(_)), "{handle}: {err}");
    }
    let after_foreign =
        case_store::list_config_deltas_for_entity(&mut conn, "ent-router", None, Utc::now())
            .await
            .unwrap();
    assert_eq!(before_foreign, after_foreign, "nothing changed");
}

#[tokio::test]
async fn session_clear_resets_the_intent_and_the_active_case_and_never_the_latch() {
    let _root = TestRoot::new().await;
    let mut conn = open(PERSONA).await;
    let case = open_a_case(&mut conn, FOCUS, "run-1").await;
    let other_case = open_a_case(&mut conn, OTHER_FOCUS, "run-2").await;
    let put = |name: &'static str, var: &'static str| {
        op(
            "session_put",
            Some(name),
            Some(serde_json::json!({ "value": var })),
        )
    };
    let v = vars(&[("i", "billing"), ("c", &case), ("oc", &other_case)]);
    for (focus, name, var) in [
        (FOCUS, "locked_intent", "i"),
        (FOCUS, "active_case_id", "c"),
        (OTHER_FOCUS, "locked_intent", "i"),
        (OTHER_FOCUS, "active_case_id", "oc"),
    ] {
        run(&mut conn, &ctx(focus, "run-3", "put"), &put(name, var), &v)
            .await
            .unwrap();
    }
    // Another chat has a latch and an active case of its own.
    session_store::latch_write_suppressed(&mut conn, "chat-other", FOCUS, "run-x")
        .await
        .unwrap();
    let clear = op("session_clear", None, None);

    let d = run(&mut conn, &ctx(FOCUS, "run-4", "clear"), &clear, &vars(&[]))
        .await
        .unwrap();
    assert_eq!(d.outcome, Outcome::Applied);
    async fn get(
        conn: &mut SqliteConnection,
        focus: &str,
        name: &str,
    ) -> Option<session_store::SessionVariable> {
        session_store::get_session_variable(conn, CHAT, focus, name)
            .await
            .unwrap()
    }
    assert!(get(&mut conn, FOCUS, "locked_intent").await.is_none());
    assert!(get(&mut conn, FOCUS, "active_case_id").await.is_none());
    // Another Focus's rows in this chat are untouched.
    assert!(get(&mut conn, OTHER_FOCUS, "locked_intent").await.is_some());
    assert!(get(&mut conn, OTHER_FOCUS, "active_case_id")
        .await
        .is_some());
    // The case itself is untouched: clearing the pointer closes nothing.
    assert!(case_store::get_case(&mut conn, &case)
        .await
        .unwrap()
        .is_some());
    // Another chat's latch is untouched.
    assert_eq!(
        session_store::write_mode(&mut conn, "chat-other")
            .await
            .unwrap(),
        WriteMode::Suppressed
    );
    // A second clear has nothing to do.
    let d = run(&mut conn, &ctx(FOCUS, "run-5", "clear"), &clear, &vars(&[]))
        .await
        .unwrap();
    assert_eq!(d.outcome, Outcome::AlreadyApplied);

    // The latch is not in the list of what it may remove, and a latched chat
    // refuses the writer instead (every_writer covers that), so the chat's own
    // latch cannot be cleared through here.
    assert!(!session_store::CLEARABLE_SESSION_NAMES.contains(&session_store::WRITE_MODE_NAME));
    assert_eq!(
        session_store::CLEARABLE_SESSION_NAMES,
        [
            crate::conductor::bound_spec::LOCKED_INTENT,
            crate::conductor::bound_spec::ACTIVE_CASE_ID
        ],
        "the clearable names are exactly the two session names"
    );
    let source = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/persistence/session_store.rs"),
    )
    .unwrap();
    let production = source.split("#[cfg(test)]").next().unwrap();
    let flat = production.split_whitespace().collect::<Vec<_>>().join(" ");
    let delete = flat
        .match_indices("DELETE FROM session_variables")
        .map(|(i, _)| &flat[i..i + 200])
        .collect::<Vec<_>>();
    assert_eq!(delete.len(), 1, "one delete in the store: {delete:?}");
    assert!(
        delete[0].contains("AND name <> ?") && delete[0].contains("name IN (?, ?)"),
        "the delete names its two variables and excludes the latch: {}",
        delete[0]
    );
}

#[tokio::test]
async fn the_new_operations_are_isolated_per_persona() {
    let _root = TestRoot::new().await;
    let mut one = open("persona-one").await;
    let mut two = open("persona-two").await;
    let case = open_a_case(&mut one, FOCUS, "run-1").await;
    let delta = record_a_delta(&mut one, FOCUS, "run-2", r#"["ent-router"]"#).await;
    run(
        &mut one,
        &ctx(FOCUS, "run-3", "sel"),
        &select_op(),
        &vars(&[("h", &handles::case_handle(FOCUS, &case))]),
    )
    .await
    .unwrap();

    // Persona one's handles resolve to nothing in persona two's file.
    let err = run(
        &mut two,
        &ctx(FOCUS, "run-4", "sel"),
        &select_op(),
        &vars(&[("h", &handles::case_handle(FOCUS, &case))]),
    )
    .await
    .unwrap_err();
    assert!(
        matches!(err, StoreWriteError::Handle(HandleError::NotFound(_))),
        "{err}"
    );
    let err = run(
        &mut two,
        &ctx(FOCUS, "run-4", "conf"),
        &op(
            "delta_confirm",
            None,
            Some(serde_json::json!({"handle": "h"})),
        ),
        &vars(&[("h", &handles::delta_handle(FOCUS, &delta))]),
    )
    .await
    .unwrap_err();
    assert!(
        matches!(err, StoreWriteError::Handle(HandleError::NotFound(_))),
        "{err}"
    );
    // Clearing persona two's session state leaves persona one's alone.
    run(
        &mut two,
        &ctx(FOCUS, "run-5", "clear"),
        &op("session_clear", None, None),
        &vars(&[]),
    )
    .await
    .unwrap();
    assert!(active_case(&mut one, FOCUS).await.is_some());
    assert_eq!(row_counts(&mut two).await, [0, 0, 0, 0, 0, 0]);
}

/// A guard-false row writes nothing, and the new operations are no exception:
/// the guard is decided before a handler is reached (lifecycle execute tests),
/// and a refused handle leaves no partial write behind.
#[tokio::test]
async fn a_failed_handle_resolution_leaves_no_partial_write_behind() {
    let _root = TestRoot::new().await;
    let mut conn = open(PERSONA).await;
    let case = open_a_case(&mut conn, FOCUS, "run-1").await;
    let delta = record_a_delta(&mut conn, FOCUS, "run-2", r#"["ent-router"]"#).await;
    let before = row_counts(&mut conn).await;
    let stored =
        case_store::list_config_deltas_for_entity(&mut conn, "ent-router", Some(FOCUS), Utc::now())
            .await
            .unwrap();
    run(
        &mut conn,
        &ctx(FOCUS, "run-3", "sel"),
        &select_op(),
        &vars(&[("h", "T-ZZZZZZZZ")]),
    )
    .await
    .unwrap_err();
    run(
        &mut conn,
        &ctx(FOCUS, "run-3", "conf"),
        &op(
            "delta_confirm",
            None,
            Some(serde_json::json!({"handle": "h"})),
        ),
        &vars(&[("h", "D-ZZZZZZZZ")]),
    )
    .await
    .unwrap_err();
    assert_eq!(row_counts(&mut conn).await, before);
    assert_eq!(
        case_store::list_config_deltas_for_entity(&mut conn, "ent-router", Some(FOCUS), Utc::now())
            .await
            .unwrap(),
        stored
    );
    // And the real handles still work afterwards.
    run(
        &mut conn,
        &ctx(FOCUS, "run-4", "sel"),
        &select_op(),
        &vars(&[("h", &handles::case_handle(FOCUS, &case))]),
    )
    .await
    .unwrap();
    let _ = delta;
}

// ---------------------------------------------------------------------------
// Source scans
// ---------------------------------------------------------------------------

fn writer_source() -> String {
    let path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/conductor/store_writer.rs");
    let text = std::fs::read_to_string(path).expect("store_writer.rs readable");
    // Production code only: the file has no test module, but keep the rule.
    text.split("#[cfg(test)]").next().unwrap().to_owned()
}

/// The 510 clock is deliberately not clamped, so the one safe writer never
/// passes an explicit `at`. There is exactly one call, and its last argument is
/// None.
#[test]
fn the_status_writer_never_passes_an_explicit_at() {
    let src = writer_source();
    let flat = src.split_whitespace().collect::<Vec<_>>().join(" ");
    let needle = "set_delta_entity_status(";
    let calls: Vec<usize> = flat.match_indices(needle).map(|(i, _)| i).collect();
    // One call, and the module header's mention is not followed by an open paren.
    assert_eq!(calls.len(), 1, "exactly one call site: {calls:?}");
    let rest = &flat[calls[0] + needle.len()..];
    let args = &rest[..rest.find(')').expect("call closes")];
    assert!(
        args.trim_end().trim_end_matches(',').ends_with(", None"),
        "the last argument (at) must be None: {args}"
    );
    // No other source of a timestamp reaches a status call.
    assert!(!args.contains("Some("), "{args}");
}

/// The handlers call typed store functions only: no SQL of their own, so every
/// write goes through the latch gate, the version compare-and-set and the
/// closed-case rule in persistence/.
#[test]
fn the_handlers_write_no_sql_of_their_own() {
    let src = writer_source().to_lowercase();
    for forbidden in [
        "sqlx::query",
        "insert into",
        "delete from",
        "begin immediate",
        "replace into",
        "update session_",
        "update case_",
        "update config_",
    ] {
        let hits: Vec<_> = src
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .filter(|l| l.contains(forbidden))
            .collect();
        assert!(
            hits.is_empty(),
            "`{forbidden}` in store_writer.rs: {hits:?}"
        );
    }
}
