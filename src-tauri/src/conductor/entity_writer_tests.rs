// src-tauri/src/conductor/entity_writer_tests.rs
//
// items.id=891 stage 3: the Focus-owned entity writers (entity_find_or_create,
// entity_fact_put) and the entity-id verification of the four writers that carry
// an id, against REAL encrypted outputs.db and personal.db files (a QR_DATA_ROOT
// scratch dir, real migrations, the production run_store_op and the production
// owned_entity_store underneath it). LOAD validation is tested beside the loader
// (store_op.rs, lifecycle.rs).

use std::collections::HashMap;

use sqlx::SqliteConnection;

use crate::conductor::store_op::{Op, StoreOp};
use crate::conductor::store_writer::{
    run_store_op, Collection, Done, Entities, Outcome, StoreWriteError, WriterContext,
};
use crate::persistence::handles;
use crate::persistence::output_store::open_outputs_db;
use crate::persistence::owned_entity_store::{self, OwnedEntityError};
use crate::persistence::personal_store::open_personal_db;
use crate::persistence::session_store::{self, WriteMode};

const KEY_HEX: &str = "deadbeef00112233445566778899aabbccddeeff00112233445566778899aabb";
const USER: &str = "u-entity";
const PERSONA: &str = "p-entity";
const CHAT: &str = "chat-e";
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

struct Db {
    outputs: SqliteConnection,
    personal: SqliteConnection,
}

async fn dbs() -> Db {
    Db {
        outputs: open_outputs_db(USER, PERSONA, KEY_HEX).await.unwrap(),
        personal: open_personal_db(USER, PERSONA, KEY_HEX).await.unwrap(),
    }
}

fn op(name: &str, target: Option<&str>, args: serde_json::Value) -> StoreOp {
    let args = args.to_string();
    StoreOp::from_columns(Some(name), target, Some(&args))
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
        persona_id: PERSONA,
        focus_id: focus,
        run_id: run,
        action_id: action,
        collection: Collection::Granted,
        user_id: "",
        data_key: None,
        turn_text: None,
    }
}

fn find_op() -> StoreOp {
    op(
        "entity_find_or_create",
        Some("ent"),
        serde_json::json!({"label": "label", "handle": "handle", "class": "class"}),
    )
}

fn put_op() -> StoreOp {
    op(
        "entity_fact_put",
        None,
        serde_json::json!({"entity": "ent", "field": "field", "value": "value"}),
    )
}

async fn go(
    db: &mut Db,
    c: &WriterContext<'_>,
    o: &StoreOp,
    v: &HashMap<String, String>,
) -> Result<Done, StoreWriteError> {
    run_store_op(
        &mut db.outputs,
        Entities::Verified(&mut db.personal),
        c,
        o,
        v,
    )
    .await
}

/// find_or_create with just a label, returning the published entity id.
async fn make(db: &mut Db, focus: &str, run: &str, label: &str) -> String {
    let d = go(
        db,
        &ctx(focus, run, "find"),
        &find_op(),
        &vars(&[("label", label), ("handle", ""), ("class", "")]),
    )
    .await
    .expect("entity_find_or_create");
    d.published.expect("published the entity id").1
}

async fn count(conn: &mut SqliteConnection, table: &str) -> i64 {
    sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {table}"))
        .fetch_one(&mut *conn)
        .await
        .unwrap()
}

async fn counts(db: &mut Db) -> Vec<i64> {
    vec![
        count(&mut db.personal, "entities").await,
        count(&mut db.personal, "entity_facts").await,
        count(&mut db.outputs, "session_variables").await,
        count(&mut db.outputs, "case_records").await,
        count(&mut db.outputs, "config_deltas").await,
    ]
}

// ---------------------------------------------------------------------------
// entity_find_or_create
// ---------------------------------------------------------------------------

#[tokio::test]
async fn find_or_create_creates_an_owned_device_publishes_its_id_and_replays() {
    let _root = TestRoot::new().await;
    let mut db = dbs().await;
    let v = vars(&[
        ("label", "Living Room Router"),
        ("handle", ""),
        ("class", "device:network:router"),
    ]);

    let d = go(&mut db, &ctx(FOCUS, "run-1", "find"), &find_op(), &v)
        .await
        .unwrap();
    assert_eq!(d.outcome, Outcome::Applied);
    let (var, id) = d.published.clone().expect("published");
    assert_eq!(var, "ent");

    let e = owned_entity_store::get_owned_active(&mut db.personal, FOCUS, &id)
        .await
        .unwrap();
    assert_eq!(e.display_name, "Living Room Router");
    assert_eq!(e.owner_focus.as_deref(), Some(FOCUS));
    let class: String = sqlx::query_scalar(
        "SELECT CAST(field_value AS TEXT) FROM entity_facts
         WHERE entity_id = ? AND field_name = 'subject_class'",
    )
    .bind(&id)
    .fetch_one(&mut db.personal)
    .await
    .unwrap();
    assert_eq!(class, "device:network:router");

    // A re-run of the same row (retry, resume) finds it: nothing new is written.
    let again = go(&mut db, &ctx(FOCUS, "run-1", "find"), &find_op(), &v)
        .await
        .unwrap();
    assert_eq!(again.outcome, Outcome::AlreadyApplied);
    assert_eq!(again.published, d.published);
    assert_eq!(count(&mut db.personal, "entities").await, 1);
    assert_eq!(count(&mut db.personal, "entity_facts").await, 1);

    // The same replay even if a model re-run named the device differently.
    let renamed = vars(&[("label", "Some Other Words"), ("handle", ""), ("class", "")]);
    let again = go(&mut db, &ctx(FOCUS, "run-1", "find"), &find_op(), &renamed)
        .await
        .unwrap();
    assert_eq!(again.published, d.published);
    assert_eq!(count(&mut db.personal, "entities").await, 1);
}

#[tokio::test]
async fn a_name_finds_the_existing_device_instead_of_making_a_second() {
    let _root = TestRoot::new().await;
    let mut db = dbs().await;
    let first = make(&mut db, FOCUS, "run-1", "Living Room Router").await;
    let second = make(&mut db, FOCUS, "run-2", "  living   ROOM router ").await;
    assert_eq!(first, second);
    assert_eq!(count(&mut db.personal, "entities").await, 1);
    // Another Focus asking for the same name gets its own device.
    let theirs = make(&mut db, OTHER_FOCUS, "run-3", "Living Room Router").await;
    assert_ne!(theirs, first);
    assert_eq!(count(&mut db.personal, "entities").await, 2);
}

#[tokio::test]
async fn a_handle_picks_an_existing_device_and_only_one_this_focus_owns() {
    let _root = TestRoot::new().await;
    let mut db = dbs().await;
    let mine = make(&mut db, FOCUS, "run-1", "Router").await;
    let theirs = make(&mut db, OTHER_FOCUS, "run-2", "Camera").await;
    let unowned = crate::persistence::entity_store::create_entity_conn(
        &mut db.personal,
        "person",
        "Sam",
        &[],
        None,
        None,
    )
    .await
    .unwrap();

    // Its own handle, however a model echoes it: found, not created.
    let h = handles::entity_handle(FOCUS, &mine);
    for echoed in [
        h.clone(),
        h.to_lowercase(),
        format!("[{h}]"),
        format!("  {h}\n"),
    ] {
        let d = go(
            &mut db,
            &ctx(FOCUS, "run-4", "find"),
            &find_op(),
            &vars(&[("label", "A new label"), ("handle", &echoed), ("class", "")]),
        )
        .await
        .unwrap();
        assert_eq!(d.outcome, Outcome::AlreadyApplied, "{echoed}");
        assert_eq!(d.published.unwrap().1, mine);
    }
    assert_eq!(
        count(&mut db.personal, "entities").await,
        3,
        "nothing was made"
    );

    // Handle-shaped but another Focus's device, an unowned entity's, or
    // invented: an error (a stale or invented handle fails the pass), and nothing
    // is created.
    for bad in [
        handles::entity_handle(OTHER_FOCUS, &theirs),
        handles::entity_handle(FOCUS, &theirs),
        handles::entity_handle(FOCUS, &unowned),
        "E-00000000".to_owned(),
    ] {
        let err = go(
            &mut db,
            &ctx(FOCUS, "run-5", "find"),
            &find_op(),
            &vars(&[("label", "Brand New"), ("handle", &bad), ("class", "")]),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, StoreWriteError::Handle(_)), "{bad}: {err}");
        assert!(!err.to_string().contains("E-0000"), "no value in {err}");
    }
    assert_eq!(count(&mut db.personal, "entities").await, 3);

    // Text that is not handle-shaped (a model saying none) is not a handle: the
    // row goes on to the name and creates the device.
    for not_a_handle in ["none", "NEW", "T-ABCDEFGH", ""] {
        let d = go(
            &mut db,
            &ctx(FOCUS, "run-6", &format!("find-{not_a_handle}")),
            &find_op(),
            &vars(&[
                ("label", &format!("Gadget {not_a_handle}")),
                ("handle", not_a_handle),
                ("class", ""),
            ]),
        )
        .await
        .unwrap();
        assert_eq!(d.outcome, Outcome::Applied, "{not_a_handle:?}");
    }
}

#[tokio::test]
async fn a_bad_class_or_name_fails_the_pass_and_writes_nothing() {
    let _root = TestRoot::new().await;
    let mut db = dbs().await;
    for (label, class) in [("Router", "Not A Class"), ("   ", ""), ("a\0b", "")] {
        let err = go(
            &mut db,
            &ctx(FOCUS, "run-1", "find"),
            &find_op(),
            &vars(&[("label", label), ("handle", ""), ("class", class)]),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(
                err,
                StoreWriteError::Entity(_) | StoreWriteError::ArgumentEmpty { .. }
            ),
            "{label:?}/{class:?}: {err}"
        );
    }
    assert_eq!(counts(&mut db).await, vec![0, 0, 0, 0, 0]);
}

// ---------------------------------------------------------------------------
// entity_fact_put
// ---------------------------------------------------------------------------

async fn put(
    db: &mut Db,
    focus: &str,
    entity: &str,
    field: &str,
    value: &str,
) -> Result<Done, StoreWriteError> {
    go(
        db,
        &ctx(focus, "run-9", "put"),
        &put_op(),
        &vars(&[("ent", entity), ("field", field), ("value", value)]),
    )
    .await
}

#[tokio::test]
async fn a_fact_put_writes_replays_and_supersedes() {
    let _root = TestRoot::new().await;
    let mut db = dbs().await;
    let id = make(&mut db, FOCUS, "run-1", "Router").await;

    assert_eq!(
        put(&mut db, FOCUS, &id, "firmware_version", "1.0")
            .await
            .unwrap()
            .outcome,
        Outcome::Applied
    );
    assert_eq!(
        put(&mut db, FOCUS, &id, "firmware_version", "1.0")
            .await
            .unwrap()
            .outcome,
        Outcome::AlreadyApplied,
        "a replay writes nothing"
    );
    assert_eq!(count(&mut db.personal, "entity_facts").await, 1);
    assert_eq!(
        put(&mut db, FOCUS, &id, "firmware_version", "2.0")
            .await
            .unwrap()
            .outcome,
        Outcome::Applied
    );
    assert_eq!(
        count(&mut db.personal, "entity_facts").await,
        2,
        "history kept"
    );
    let facts = owned_entity_store::list_active_facts(&mut db.personal, FOCUS, &id)
        .await
        .unwrap();
    assert_eq!(facts.len(), 1);
    assert_eq!(facts[0].value, "2.0");
}

#[tokio::test]
async fn an_unknown_field_a_bad_value_or_a_wrong_entity_fails_the_pass_closed() {
    let _root = TestRoot::new().await;
    let mut db = dbs().await;
    let mine = make(&mut db, FOCUS, "run-1", "Router").await;
    let theirs = make(&mut db, OTHER_FOCUS, "run-2", "Camera").await;
    let before = counts(&mut db).await;

    for (focus, entity, field, value, what) in [
        (
            FOCUS,
            mine.as_str(),
            "wifi_password",
            "hunter2",
            "a credential field",
        ),
        (
            FOCUS,
            mine.as_str(),
            "notes",
            "free text",
            "a field outside the catalogue",
        ),
        (FOCUS, mine.as_str(), "make", "a\nb", "a control character"),
        (
            FOCUS,
            theirs.as_str(),
            "make",
            "Netgear",
            "another Focus's device",
        ),
        (
            OTHER_FOCUS,
            mine.as_str(),
            "make",
            "Netgear",
            "another Focus asking",
        ),
        (FOCUS, "not-an-entity", "make", "Netgear", "an invented id"),
    ] {
        let err = put(&mut db, focus, entity, field, value).await.unwrap_err();
        assert!(matches!(err, StoreWriteError::Entity(_)), "{what}: {err}");
        assert!(!err.to_string().contains("hunter2"), "{what}: {err}");
    }
    // An empty value is the writer's own empty-slot error.
    assert!(matches!(
        put(&mut db, FOCUS, &mine, "make", "   ").await.unwrap_err(),
        StoreWriteError::ArgumentEmpty { .. }
    ));
    assert_eq!(counts(&mut db).await, before, "every refusal wrote nothing");
}

#[tokio::test]
async fn a_purge_marked_device_takes_no_write() {
    let _root = TestRoot::new().await;
    let mut db = dbs().await;
    let id = make(&mut db, FOCUS, "run-1", "Router").await;
    sqlx::query("UPDATE entities SET status = 'user_deleted' WHERE id = ?")
        .bind(&id)
        .execute(&mut db.personal)
        .await
        .unwrap();
    let err = put(&mut db, FOCUS, &id, "make", "Netgear")
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        StoreWriteError::Entity(OwnedEntityError::NotFound)
    ));
    // Nor does a name or a handle find it again: a new device is made.
    let again = make(&mut db, FOCUS, "run-2", "Router").await;
    assert_ne!(again, id);
}

// ---------------------------------------------------------------------------
// The one-time latch and the consent gate
// ---------------------------------------------------------------------------

async fn latch(db: &mut Db) {
    let o = StoreOp::from_columns(Some("latch"), None, None)
        .unwrap()
        .unwrap();
    run_store_op(
        &mut db.outputs,
        Entities::NotNeeded,
        &ctx(FOCUS, "run-l", "latch"),
        &o,
        &vars(&[]),
    )
    .await
    .unwrap();
    assert_eq!(
        session_store::write_mode(&mut db.outputs, CHAT)
            .await
            .unwrap(),
        WriteMode::Suppressed
    );
}

#[tokio::test]
async fn a_one_time_chat_creates_no_entity_and_writes_no_fact() {
    let _root = TestRoot::new().await;
    let mut db = dbs().await;
    let id = make(&mut db, FOCUS, "run-1", "Router").await;
    latch(&mut db).await;
    let before = counts(&mut db).await;

    let d = go(
        &mut db,
        &ctx(FOCUS, "run-2", "find"),
        &find_op(),
        &vars(&[("label", "Phone"), ("handle", ""), ("class", "")]),
    )
    .await
    .unwrap();
    assert_eq!(d.outcome, Outcome::RefusedOneTime);
    assert_eq!(d.published, None);
    let d = put(&mut db, FOCUS, &id, "make", "Netgear").await.unwrap();
    assert_eq!(d.outcome, Outcome::RefusedOneTime);
    assert_eq!(counts(&mut db).await, before);

    // A row whose arguments would not even resolve is still a refusal in a latched
    // chat, never an error.
    let d = put(&mut db, FOCUS, "not-an-entity", "wifi_password", "x")
        .await
        .unwrap();
    assert_eq!(d.outcome, Outcome::RefusedOneTime);
}

/// Without the current consent BOTH entity operations are refused, write nothing
/// in either database, publish nothing, and are not errors. The consent check is
/// first: it succeeds even when the entity store is not available at all.
#[tokio::test]
async fn without_consent_both_entity_operations_are_refused_and_write_nothing() {
    let _root = TestRoot::new().await;
    let mut db = dbs().await;
    let id = make(&mut db, FOCUS, "run-1", "Router").await;
    let before = counts(&mut db).await;
    let denied = |run, action| WriterContext {
        collection: Collection::NotGranted,
        ..ctx(FOCUS, run, action)
    };

    for (o, v) in [
        (
            find_op(),
            vars(&[("label", "Phone"), ("handle", ""), ("class", "")]),
        ),
        (
            put_op(),
            vars(&[("ent", &id), ("field", "make"), ("value", "Netgear")]),
        ),
    ] {
        assert!(o.op.requires_collection_consent());
        let d = go(&mut db, &denied("run-2", "w"), &o, &v).await.unwrap();
        assert_eq!(d.outcome, Outcome::RefusedNoConsent, "{}", o.op.as_str());
        assert_eq!(d.published, None);
        // And with no entity store at all.
        let d = run_store_op(
            &mut db.outputs,
            Entities::NotNeeded,
            &denied("run-2", "w"),
            &o,
            &v,
        )
        .await
        .unwrap();
        assert_eq!(d.outcome, Outcome::RefusedNoConsent);
    }
    assert_eq!(counts(&mut db).await, before);

    // Consent first, then the latch: a latched chat without consent reads as no
    // consent, exactly as for the other writers.
    latch(&mut db).await;
    let d = go(
        &mut db,
        &denied("run-3", "w"),
        &find_op(),
        &vars(&[("label", "Phone"), ("handle", ""), ("class", "")]),
    )
    .await
    .unwrap();
    assert_eq!(d.outcome, Outcome::RefusedNoConsent);
}

/// Every operation of the closed set is classified, and exactly these two
/// entity operations run outside the outputs.db-only path.
#[test]
fn the_entity_operations_are_the_only_ones_that_need_the_entity_store() {
    for o in Op::ALL {
        let spec_needs = o.is_entity_op();
        assert_eq!(
            spec_needs,
            matches!(o, Op::EntityFindOrCreate | Op::EntityFactPut),
            "{}",
            o.as_str()
        );
    }
}

// ---------------------------------------------------------------------------
// Entity ids named by the other writers
// ---------------------------------------------------------------------------

fn open_case_op() -> StoreOp {
    op(
        "case_open",
        Some("new_case"),
        serde_json::json!({"entity": "ent", "label": "l"}),
    )
}

#[tokio::test]
async fn a_case_a_delta_and_a_link_name_only_a_device_this_focus_owns() {
    let _root = TestRoot::new().await;
    let mut db = dbs().await;
    let mine = make(&mut db, FOCUS, "run-1", "Router").await;
    let theirs = make(&mut db, OTHER_FOCUS, "run-2", "Camera").await;

    // case_open on its own device: applied.
    let d = go(
        &mut db,
        &ctx(FOCUS, "run-3", "open"),
        &open_case_op(),
        &vars(&[("ent", &mine), ("l", "Router")]),
    )
    .await
    .unwrap();
    assert_eq!(d.outcome, Outcome::Applied);
    let case_id = d.published.unwrap().1;

    // delta_record takes ONE published id as its list.
    let rec = op(
        "delta_record",
        Some("new_delta"),
        serde_json::json!({"entities": "ent", "description": "d"}),
    );
    let d = go(
        &mut db,
        &ctx(FOCUS, "run-3", "rec"),
        &rec,
        &vars(&[("ent", &mine), ("d", "changed DNS")]),
    )
    .await
    .unwrap();
    assert_eq!(d.outcome, Outcome::Applied);

    // case_link and delta_changed on a second owned device.
    let second = make(&mut db, FOCUS, "run-4", "Modem").await;
    let link = op(
        "case_link",
        None,
        serde_json::json!({"case": "c", "entity": "ent"}),
    );
    let d = go(
        &mut db,
        &ctx(FOCUS, "run-5", "link"),
        &link,
        &vars(&[("c", &case_id), ("ent", &second)]),
    )
    .await
    .unwrap();
    assert_eq!(d.outcome, Outcome::Applied);

    // Another Focus's device, an invented id and a purge-marked device are each
    // refused by every writer that names an entity, with nothing written.
    sqlx::query("UPDATE entities SET status = 'user_deleted' WHERE id = ?")
        .bind(&second)
        .execute(&mut db.personal)
        .await
        .unwrap();
    let before = counts(&mut db).await;
    let case_entities_before = count(&mut db.outputs, "case_entities").await;
    for (bad, what) in [
        (theirs.as_str(), "another Focus's device"),
        ("not-an-entity", "an invented id"),
        (second.as_str(), "a purge-marked device"),
    ] {
        let rows: Vec<(&str, StoreOp, HashMap<String, String>)> = vec![
            (
                "case_open",
                open_case_op(),
                vars(&[("ent", bad), ("l", "x")]),
            ),
            (
                "case_link",
                link.clone(),
                vars(&[("c", &case_id), ("ent", bad)]),
            ),
            (
                "delta_record",
                rec.clone(),
                vars(&[("ent", bad), ("d", "x")]),
            ),
            (
                "delta_changed",
                op(
                    "delta_changed",
                    None,
                    serde_json::json!({"delta": "dl", "entity": "ent"}),
                ),
                vars(&[("dl", "run-3:rec"), ("ent", bad)]),
            ),
        ];
        for (name, o, v) in rows {
            let err = go(&mut db, &ctx(FOCUS, "run-6", "bad"), &o, &v)
                .await
                .unwrap_err();
            assert!(
                matches!(err, StoreWriteError::Entity(OwnedEntityError::NotFound)),
                "{name} with {what}: {err}"
            );
        }
    }
    assert_eq!(counts(&mut db).await, before);
    assert_eq!(
        count(&mut db.outputs, "case_entities").await,
        case_entities_before
    );
}

#[tokio::test]
async fn a_latched_chat_refuses_a_case_writer_even_with_a_bad_entity() {
    let _root = TestRoot::new().await;
    let mut db = dbs().await;
    latch(&mut db).await;
    let d = go(
        &mut db,
        &ctx(FOCUS, "run-1", "open"),
        &open_case_op(),
        &vars(&[("ent", "not-an-entity"), ("l", "x")]),
    )
    .await
    .unwrap();
    assert_eq!(d.outcome, Outcome::RefusedOneTime);
}

#[tokio::test]
async fn an_operation_that_needs_the_entity_store_fails_closed_without_it() {
    let _root = TestRoot::new().await;
    let mut db = dbs().await;
    let v = vars(&[("label", "Router"), ("handle", ""), ("class", "")]);
    for entities in [Entities::NotNeeded, Entities::Unchecked] {
        let err = run_store_op(
            &mut db.outputs,
            entities,
            &ctx(FOCUS, "run-1", "find"),
            &find_op(),
            &v,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, StoreWriteError::EntityStoreUnavailable));
    }
    // case_open names an id: NotNeeded fails closed, Unchecked is the test-only
    // escape.
    let err = run_store_op(
        &mut db.outputs,
        Entities::NotNeeded,
        &ctx(FOCUS, "run-1", "open"),
        &open_case_op(),
        &vars(&[("ent", "e"), ("l", "x")]),
    )
    .await
    .unwrap_err();
    assert!(matches!(err, StoreWriteError::EntityStoreUnavailable));
    assert_eq!(count(&mut db.personal, "entities").await, 0);
    assert_eq!(count(&mut db.outputs, "case_records").await, 0);
}

// ---------------------------------------------------------------------------
// Source scans
// ---------------------------------------------------------------------------

fn production_source(rel: &str) -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(rel);
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{rel}: {e}"));
    text.split("\n#[cfg(test)]").next().unwrap().to_owned()
}

fn code_lines_containing(src: &str, needle: &str) -> Vec<String> {
    let needle = needle.to_lowercase();
    src.lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .filter(|l| l.to_lowercase().contains(&needle))
        .map(str::to_owned)
        .collect()
}

/// The entity writer deletes nothing and writes no SQL of its own: only
/// entity_purge deletes an owned entity (stage 4).
#[test]
fn the_entity_writer_deletes_nothing_and_writes_no_sql_of_its_own() {
    let src = production_source("src/conductor/entity_writer.rs");
    for forbidden in [
        "delete from",
        "drop table",
        "insert into",
        "update ",
        "sqlx::query",
    ] {
        let hits = code_lines_containing(&src, forbidden);
        assert!(
            hits.is_empty(),
            "`{forbidden}` in entity_writer.rs: {hits:?}"
        );
    }
}

/// The only caller of Entities::Unchecked is a test: the lifecycle supplies the
/// persona's personal.db connection or says NotNeeded.
#[test]
fn no_production_code_names_the_unchecked_entity_store() {
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
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    walk(&root, &mut files);
    assert!(files.len() > 50);
    for file in files {
        let text = std::fs::read_to_string(&file).unwrap();
        let production = text.split("\n#[cfg(test)]").next().unwrap();
        let rel = file
            .strip_prefix(&root)
            .unwrap()
            .to_string_lossy()
            .to_string();
        if rel.ends_with("_tests.rs") {
            continue;
        }
        let hits: Vec<String> = code_lines_containing(production, "Entities::Unchecked");
        assert!(
            hits.is_empty() || rel == "conductor/store_writer.rs",
            "{rel}: {hits:?}"
        );
    }
    // store_writer.rs matches on it and never constructs it.
    let writer = production_source("src/conductor/store_writer.rs");
    assert!(code_lines_containing(&writer, "Entities::Unchecked")
        .iter()
        .all(|l| l.contains("=>")));
    // The lifecycle supplies the connection.
    let lifecycle = production_source("src/conductor/lifecycle.rs");
    assert!(!code_lines_containing(&lifecycle, "Entities::Verified").is_empty());
    assert!(!code_lines_containing(&lifecycle, "Entities::NotNeeded").is_empty());
}
