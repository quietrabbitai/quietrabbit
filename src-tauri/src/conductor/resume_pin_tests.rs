// items.id=888 stage 5: the checkpoint pin of the chat's stores and its check on resume.

use std::collections::{BTreeMap, HashMap};

use sqlx::SqliteConnection;

use super::*;
use crate::conductor::store_op::StoreOp;
use crate::conductor::tokens::{ExternalAccess, StepType};
use crate::persistence::output_store::open_outputs_db;
use crate::persistence::pass_artifact_store::{self, ArtifactKind, ArtifactWrite};
use crate::persistence::personal_store::open_personal_db;
use crate::persistence::session_store::{self, SessionWrite};

const KEY_HEX: &str = "a1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8f90";
const USER: &str = "u-pin";
const PERSONA: &str = "p-pin";
const FOCUS: &str = "tech-support";
const CHAT: &str = "chat-pin";

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

async fn outputs() -> SqliteConnection {
    open_outputs_db(USER, PERSONA, KEY_HEX).await.unwrap()
}

fn step(id: &str, template: &str, store_op: Option<StoreOp>) -> ActionDefinition {
    ActionDefinition {
        action_id: id.to_owned(),
        display_name: id.to_owned(),
        guide_id: "g".to_owned(),
        task_type: "general".to_owned(),
        routing_tier: ExternalAccess::LocalOnly,
        step_type: StepType::Generate,
        output_var: None,
        prompt_template: template.to_owned(),
        field_requirements: HashMap::new(),
        options_override: HashMap::new(),
        external_access_override: Some(ExternalAccess::LocalOnly),
        requires_user_handoff: false,
        revisitable: false,
        block_stable_id: None,
        block_customization: None,
        schedule_trigger: None,
        guard: None,
        declared_inputs: Vec::new(),
        store_op,
        and_guards: Vec::new(),
        requirement: None,
    }
}

fn writer(op: &str, target: &str, args: serde_json::Value) -> Option<StoreOp> {
    let parsed = StoreOp::from_columns(Some(op), Some(target), Some(&args.to_string()))
        .expect("row parses")
        .expect("a writer");
    Some(parsed)
}

async fn put_session(conn: &mut SqliteConnection, name: &str, value: &str) {
    let current = session_store::get_session_variable(conn, CHAT, FOCUS, name)
        .await
        .unwrap();
    session_store::put_session_variable(
        conn,
        &SessionWrite {
            context_key: CHAT,
            focus_id: FOCUS,
            name,
            value,
            written_by_run: "run-w",
            expected_version: current.map(|c| c.version),
        },
    )
    .await
    .unwrap();
}

#[test]
fn only_session_launch_and_artifact_names_are_versioned() {
    let steps = [step(
        "a",
        "{locked_intent} {write_mode} {content_brief} {document_type} {case_context} {user_input}",
        None,
    )];
    let mut names = versioned_names(&steps);
    names.sort_unstable();
    assert_eq!(names, ["content_brief", "document_type", "locked_intent"]);
}

#[tokio::test]
async fn versions_are_read_per_chat_and_focus_with_zero_for_no_row() {
    let _root = TestRoot::new().await;
    let mut conn = outputs().await;
    put_session(&mut conn, "locked_intent", "billing").await;
    put_session(&mut conn, "locked_intent", "refund").await;
    pass_artifact_store::put_artifact(
        &mut conn,
        &ArtifactWrite {
            context_key: CHAT,
            focus_id: FOCUS,
            kind: ArtifactKind::ContentBrief,
            content: "a brief",
            written_by_run: "run-w",
            expected_version: None,
        },
    )
    .await
    .unwrap();
    let got = read_versions(
        &mut conn,
        Some(CHAT),
        FOCUS,
        &["locked_intent", "content_brief", "active_case_id"],
    )
    .await
    .unwrap();
    let want: BTreeMap<String, i64> = [
        ("active_case_id".to_owned(), 0),
        ("content_brief".to_owned(), 1),
        ("locked_intent".to_owned(), 2),
    ]
    .into();
    assert_eq!(got, want);
    // Another chat or Focus sees no rows.
    let other = read_versions(&mut conn, Some("chat-other"), FOCUS, &["locked_intent"])
        .await
        .unwrap();
    assert_eq!(other["locked_intent"], 0);
    // No chat: nothing stored, nothing pinned.
    assert!(read_versions(&mut conn, None, FOCUS, &["locked_intent"])
        .await
        .unwrap()
        .is_empty());
    // A name that is not a versioned bound name is refused.
    assert!(
        read_versions(&mut conn, Some(CHAT), FOCUS, &["case_context"])
            .await
            .is_err()
    );
}

#[test]
fn the_metadata_round_trips_and_never_holds_a_value() {
    let versions: BTreeMap<String, i64> = [("locked_intent".to_owned(), 3)].into();
    let published: HashMap<String, String> = [("new_case".to_owned(), "case-1".to_owned())].into();
    let text = checkpoint_metadata(Some(Ok(versions.clone())), &published, None);
    assert!(!text.contains("billing"));
    let pinned = parse(Some(&text)).unwrap();
    assert_eq!(pinned.versions, Some(versions));
    assert!(!pinned.unreadable);
    assert_eq!(pinned.published["new_case"], "case-1");

    let unreadable = parse(Some(&checkpoint_metadata(
        Some(Err(PinError)),
        &HashMap::new(),
        None,
    )))
    .unwrap();
    assert!(unreadable.unreadable);
    assert_eq!(unreadable.versions, None);

    // Nothing pinned: no versioned name, no published id, or no metadata at all.
    for empty in [
        checkpoint_metadata(None, &HashMap::new(), None),
        "{}".to_owned(),
        String::new(),
    ] {
        assert_eq!(parse(Some(&empty)).unwrap(), Pinned::default(), "{empty:?}");
    }
    assert_eq!(parse(None).unwrap(), Pinned::default());

    // Anything that is not the expected shape is refused.
    for bad in [
        "not json",
        "[]",
        r#"{"seed_versions": {"locked_intent": "3"}}"#,
        r#"{"seed_versions": []}"#,
        r#"{"published": {"x": 1}}"#,
        r#"{"seed_versions_unreadable": "yes"}"#,
    ] {
        assert!(parse(Some(bad)).is_err(), "{bad}");
    }
}

/// Every kind of published id is verified against its own store, scoped to the Focus;
/// one that is gone, another Focus's, archived, or published by no row fails.
#[tokio::test]
async fn published_ids_are_verified_in_their_own_store() {
    let _root = TestRoot::new().await;
    let mut conn = outputs().await;
    let mut personal = open_personal_db(USER, PERSONA, KEY_HEX).await.unwrap();
    let entity = crate::persistence::owned_entity_store::create_subject(
        &mut personal,
        FOCUS,
        PERSONA,
        "Router",
        None,
        "run-1:ent",
    )
    .await
    .unwrap();
    for (case, focus) in [("case-ours", FOCUS), ("case-theirs", "other-focus")] {
        sqlx::query(
            "INSERT INTO case_records (case_id, opened_by_run, opened_at, updated_at, focus_id)
             VALUES (?, 'run-1', 't', 't', ?)",
        )
        .bind(case)
        .bind(focus)
        .execute(&mut conn)
        .await
        .unwrap();
    }
    sqlx::query(
        "INSERT INTO config_deltas
         (delta_id, session_reference, description, changed_at, recorded_by_run, created_at,
          focus_id)
         VALUES ('delta-ours', 'run-1', 'd', 't', 'run-1', 't', ?)",
    )
    .bind(FOCUS)
    .execute(&mut conn)
    .await
    .unwrap();
    let steps = [
        step(
            "ent",
            "",
            writer(
                "entity_find_or_create",
                "ent_out",
                serde_json::json!({"label": "label_out"}),
            ),
        ),
        step(
            "open",
            "",
            writer(
                "case_open",
                "new_case",
                serde_json::json!({"entity": "ent_out", "label": "label_out"}),
            ),
        ),
        step(
            "rec",
            "",
            writer(
                "delta_record",
                "new_delta",
                serde_json::json!({"entities": "ent_out", "description": "d_out"}),
            ),
        ),
    ];
    let good: BTreeMap<String, String> = [
        ("ent_out".to_owned(), entity.clone()),
        ("new_case".to_owned(), "case-ours".to_owned()),
        ("new_delta".to_owned(), "delta-ours".to_owned()),
    ]
    .into();
    assert!(verify_published(&mut conn, Some(&mut personal), FOCUS, &steps, &good).await);

    let with = |var: &str, id: &str| {
        let mut m = good.clone();
        m.insert(var.to_owned(), id.to_owned());
        m
    };
    for (label, bad) in [
        ("another Focus's case", with("new_case", "case-theirs")),
        ("a case that is gone", with("new_case", "case-gone")),
        ("a delta that is gone", with("new_delta", "delta-gone")),
        ("an entity that is gone", with("ent_out", "ent-gone")),
        ("a variable no row publishes", with("stray", "x")),
    ] {
        assert!(
            !verify_published(&mut conn, Some(&mut personal), FOCUS, &steps, &bad).await,
            "{label}"
        );
    }
    // An entity id with no personal.db connection cannot be verified.
    assert!(!verify_published(&mut conn, None, FOCUS, &steps, &good).await);
    // An archived entity is not active any more.
    sqlx::query("UPDATE entities SET status = 'archived' WHERE id = ?")
        .bind(&entity)
        .execute(&mut personal)
        .await
        .unwrap();
    assert!(!verify_published(&mut conn, Some(&mut personal), FOCUS, &steps, &good).await);
}
