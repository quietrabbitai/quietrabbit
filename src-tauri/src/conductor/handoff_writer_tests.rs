// src-tauri/src/conductor/handoff_writer_tests.rs
//
// items.id=387 (S2): handoff_ingest against a REAL encrypted outputs.db (a
// QR_DATA_ROOT scratch dir, real migrations, the production run_store_op, the
// production draft store and ingest blob underneath it), and the source scans
// that hold the raw turn to the one writer that may read it.

use std::collections::HashMap;

use sqlx::SqliteConnection;

use crate::conductor::bound_spec::{HANDOFF_PENDING, HANDOFF_RECEIVED};
use crate::conductor::handoff_writer::{CurrentTurn, SecretHex};
use crate::conductor::store_op::StoreOp;
use crate::conductor::store_writer::{
    run_store_op, Collection, Entities, Outcome, StoreWriteError, WriterContext,
};
use crate::persistence::handles::draft_handle;
use crate::persistence::handoff_draft_store::{self, DraftError, PASTE_MAX_BYTES, SENSITIVITY};
use crate::persistence::ingest_blob;
use crate::persistence::output_store::open_outputs_db;
use crate::persistence::session_store::{self, LatchError, SessionStoreError, SessionWrite};

const KEY_HEX: &str = "deadbeef00112233445566778899aabbccddeeff00112233445566778899aabb";
const USER: &str = "u-handoff";
const PERSONA: &str = "p-handoff";
const CHAT: &str = "chat-h";
const FOCUS: &str = "writing-assistant";
const PENDING: &str = "h_0123456789ab";
const TARGET: &str = "draft_ref";

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

async fn open() -> SqliteConnection {
    open_outputs_db(USER, PERSONA, KEY_HEX)
        .await
        .expect("outputs.db")
}

fn ingest_op() -> StoreOp {
    StoreOp::from_columns(Some("handoff_ingest"), Some(TARGET), None)
        .unwrap()
        .unwrap()
}

async fn set_pending(conn: &mut SqliteConnection, chat: &str, focus: &str) {
    set_session(conn, chat, focus, HANDOFF_PENDING, PENDING).await;
}

async fn set_session(conn: &mut SqliteConnection, chat: &str, focus: &str, name: &str, v: &str) {
    session_store::put_session_variable(
        conn,
        &SessionWrite {
            context_key: chat,
            focus_id: focus,
            name,
            value: v,
            written_by_run: "out-run",
            expected_version: None,
        },
    )
    .await
    .unwrap();
}

fn ctx<'a>(turn: Option<&'a CurrentTurn>, collection: Collection) -> WriterContext<'a> {
    WriterContext {
        context_key: Some(CHAT),
        persona_id: PERSONA,
        focus_id: FOCUS,
        run_id: "back-run",
        action_id: "ingest",
        collection,
        user_id: USER,
        data_key: Some(SecretHex::new(KEY_HEX)),
        turn_text: turn.map(|t| t.lend()),
    }
}

async fn run(
    conn: &mut SqliteConnection,
    c: &WriterContext<'_>,
) -> Result<crate::conductor::store_writer::Done, StoreWriteError> {
    run_store_op(conn, Entities::NotNeeded, c, &ingest_op(), &HashMap::new()).await
}

async fn variable(conn: &mut SqliteConnection, name: &str) -> Option<String> {
    session_store::get_session_variable(conn, CHAT, FOCUS, name)
        .await
        .unwrap()
        .map(|v| v.value)
}

async fn draft_count(conn: &mut SqliteConnection) -> usize {
    handoff_draft_store::list_draft_ids(conn, FOCUS)
        .await
        .unwrap()
        .len()
}

fn blob_root() -> std::path::PathBuf {
    ingest_blob::ingest_blob_path(USER, PERSONA, "x", 1)
        .parent()
        .and_then(|p| p.parent())
        .unwrap()
        .to_path_buf()
}

/// Nothing was stored and the hand-off is still waiting.
async fn assert_nothing_stored(conn: &mut SqliteConnection) {
    assert_eq!(draft_count(conn).await, 0, "no draft row");
    assert!(!blob_root().exists(), "no blob directory");
    assert_eq!(
        variable(conn, HANDOFF_PENDING).await.as_deref(),
        Some(PENDING)
    );
    assert_eq!(variable(conn, HANDOFF_RECEIVED).await, None);
}

// -- the happy path --------------------------------------------------------------

#[tokio::test]
async fn a_return_stores_the_exact_paste_clears_pending_and_publishes_the_handle() {
    let _root = TestRoot::new().await;
    let mut conn = open().await;
    set_pending(&mut conn, CHAT, FOCUS).await;
    // Braces, edges and non-ASCII are kept exactly: nothing rewrites the paste.
    let paste = "  {a brace} Dear all,\n\n  café — thanks.\n ";
    let turn = CurrentTurn::new(paste.to_owned());

    let d = run(&mut conn, &ctx(Some(&turn), Collection::Granted))
        .await
        .unwrap();
    assert_eq!(d.outcome, Outcome::Applied);

    let ids = handoff_draft_store::list_draft_ids(&mut conn, FOCUS)
        .await
        .unwrap();
    assert_eq!(ids.len(), 1);
    let handle = draft_handle(FOCUS, &ids[0]);
    assert_eq!(d.published, Some((TARGET.to_owned(), handle.clone())));
    assert_eq!(variable(&mut conn, HANDOFF_PENDING).await, None);
    assert_eq!(
        variable(&mut conn, HANDOFF_RECEIVED).await.as_deref(),
        Some(handle.as_str())
    );
    // Read by handle: the exact text, as an untrusted draft.
    let text = handoff_draft_store::read_draft_by_handle(&mut conn, FOCUS, &handle)
        .await
        .unwrap();
    assert_eq!(text.as_str(), paste);
    let rec = crate::persistence::output_store::get_output(USER, PERSONA, KEY_HEX, &ids[0])
        .await
        .unwrap()
        .unwrap();
    assert_eq!(rec.source, "external_ingested");
    assert_eq!(rec.sensitivity, SENSITIVITY);
    assert_eq!(rec.focus_slug.as_deref(), Some(FOCUS));
}

#[tokio::test]
async fn a_second_return_needs_a_new_pending_hand_off() {
    let _root = TestRoot::new().await;
    let mut conn = open().await;
    set_pending(&mut conn, CHAT, FOCUS).await;
    let turn = CurrentTurn::new("first reply".to_owned());
    run(&mut conn, &ctx(Some(&turn), Collection::Granted))
        .await
        .unwrap();

    // Replay of the same pass: nothing is pending, so it is refused, stores
    // nothing more, and leaves the first reply as received.
    let turn2 = CurrentTurn::new("second reply".to_owned());
    let err = run(&mut conn, &ctx(Some(&turn2), Collection::Granted))
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        StoreWriteError::Session(SessionStoreError::HandoffNotPending)
    ));
    assert_eq!(draft_count(&mut conn).await, 1);

    // A new hand-off, a new reply: received moves to the newer handle.
    set_pending(&mut conn, CHAT, FOCUS).await;
    run(&mut conn, &ctx(Some(&turn2), Collection::Granted))
        .await
        .unwrap();
    assert_eq!(draft_count(&mut conn).await, 2);
    let received = variable(&mut conn, HANDOFF_RECEIVED).await.unwrap();
    let text = handoff_draft_store::read_draft_by_handle(&mut conn, FOCUS, &received)
        .await
        .unwrap();
    assert_eq!(text.as_str(), "second reply");
}

// -- consent, latch, missing inputs ----------------------------------------------

#[tokio::test]
async fn without_consent_it_is_refused_and_stores_nothing() {
    let _root = TestRoot::new().await;
    let mut conn = open().await;
    set_pending(&mut conn, CHAT, FOCUS).await;
    let turn = CurrentTurn::new("a reply".to_owned());
    let d = run(&mut conn, &ctx(Some(&turn), Collection::NotGranted))
        .await
        .unwrap();
    assert_eq!(d.outcome, Outcome::RefusedNoConsent);
    assert_eq!(d.published, None);
    assert_nothing_stored(&mut conn).await;
}

#[tokio::test]
async fn a_latched_chat_is_refused_before_anything_is_stored() {
    let _root = TestRoot::new().await;
    let mut conn = open().await;
    set_pending(&mut conn, CHAT, FOCUS).await;
    session_store::latch_write_suppressed(&mut conn, CHAT, FOCUS, "r0")
        .await
        .unwrap();
    for turn in [Some(CurrentTurn::new("a reply".to_owned())), None] {
        let d = run(&mut conn, &ctx(turn.as_ref(), Collection::Granted))
            .await
            .unwrap();
        assert_eq!(d.outcome, Outcome::RefusedOneTime);
        assert_eq!(d.published, None);
    }
    assert_eq!(draft_count(&mut conn).await, 0);
    assert!(!blob_root().exists(), "a latched chat writes no blob");
    assert_eq!(
        variable(&mut conn, HANDOFF_PENDING).await.as_deref(),
        Some(PENDING)
    );
    assert_eq!(
        session_store::write_mode(&mut conn, CHAT).await.unwrap(),
        session_store::WriteMode::Suppressed
    );
    let _ = LatchError::Suppressed;
}

#[tokio::test]
async fn without_a_turn_it_fails_closed() {
    let _root = TestRoot::new().await;
    let mut conn = open().await;
    set_pending(&mut conn, CHAT, FOCUS).await;
    let err = run(&mut conn, &ctx(None, Collection::Granted))
        .await
        .unwrap_err();
    assert!(matches!(err, StoreWriteError::HandoffTurnUnavailable));
    assert_nothing_stored(&mut conn).await;
}

#[tokio::test]
async fn without_the_user_or_the_key_it_fails_closed() {
    let _root = TestRoot::new().await;
    let mut conn = open().await;
    set_pending(&mut conn, CHAT, FOCUS).await;
    let turn = CurrentTurn::new("a reply".to_owned());
    let no_key = WriterContext {
        data_key: None,
        ..ctx(Some(&turn), Collection::Granted)
    };
    let no_user = WriterContext {
        user_id: "",
        ..ctx(Some(&turn), Collection::Granted)
    };
    for c in [no_key, no_user] {
        assert!(matches!(
            run(&mut conn, &c).await.unwrap_err(),
            StoreWriteError::HandoffNoStoreAccess
        ));
    }
    assert_nothing_stored(&mut conn).await;
}

#[tokio::test]
async fn with_no_chat_it_has_no_context_key() {
    let _root = TestRoot::new().await;
    let mut conn = open().await;
    let turn = CurrentTurn::new("a reply".to_owned());
    let c = WriterContext {
        context_key: None,
        ..ctx(Some(&turn), Collection::Granted)
    };
    assert!(matches!(
        run(&mut conn, &c).await.unwrap_err(),
        StoreWriteError::NoContextKey
    ));
    assert_eq!(draft_count(&mut conn).await, 0);
}

// -- pending ------------------------------------------------------------------------

#[tokio::test]
async fn nothing_pending_stores_nothing() {
    let _root = TestRoot::new().await;
    let mut conn = open().await;
    let turn = CurrentTurn::new("a reply".to_owned());
    assert!(matches!(
        run(&mut conn, &ctx(Some(&turn), Collection::Granted))
            .await
            .unwrap_err(),
        StoreWriteError::Session(SessionStoreError::HandoffNotPending)
    ));
    assert_eq!(draft_count(&mut conn).await, 0);
    assert!(!blob_root().exists());
}

#[tokio::test]
async fn another_focus_or_chat_pending_does_not_count() {
    let _root = TestRoot::new().await;
    let mut conn = open().await;
    set_pending(&mut conn, CHAT, "other-focus").await;
    set_pending(&mut conn, "other-chat", FOCUS).await;
    let turn = CurrentTurn::new("a reply".to_owned());
    assert!(matches!(
        run(&mut conn, &ctx(Some(&turn), Collection::Granted))
            .await
            .unwrap_err(),
        StoreWriteError::Session(SessionStoreError::HandoffNotPending)
    ));
    assert_eq!(draft_count(&mut conn).await, 0);
    // Theirs are untouched.
    assert!(
        session_store::get_session_variable(&mut conn, CHAT, "other-focus", HANDOFF_PENDING)
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn a_pending_value_that_fails_its_spec_fails_the_pass() {
    let _root = TestRoot::new().await;
    let mut conn = open().await;
    set_session(&mut conn, CHAT, FOCUS, HANDOFF_PENDING, "not-an-id").await;
    let turn = CurrentTurn::new("a reply".to_owned());
    assert!(matches!(
        run(&mut conn, &ctx(Some(&turn), Collection::Granted))
            .await
            .unwrap_err(),
        StoreWriteError::InvalidSessionValue(_)
    ));
    assert_eq!(draft_count(&mut conn).await, 0);
}

// -- caps -----------------------------------------------------------------------------

#[tokio::test]
async fn a_blank_nul_or_oversize_paste_is_refused_and_stores_nothing() {
    let _root = TestRoot::new().await;
    let mut conn = open().await;
    set_pending(&mut conn, CHAT, FOCUS).await;
    for (paste, expect) in [
        (String::new(), "blank"),
        ("  \n\t".to_owned(), "blank"),
        ("a\0b".to_owned(), "nul"),
        ("a".repeat(PASTE_MAX_BYTES + 1), "large"),
    ] {
        let turn = CurrentTurn::new(paste);
        let err = run(&mut conn, &ctx(Some(&turn), Collection::Granted))
            .await
            .unwrap_err();
        let ok = matches!(
            (&err, expect),
            (StoreWriteError::Draft(DraftError::Blank), "blank")
                | (StoreWriteError::Draft(DraftError::ContainsNul), "nul")
                | (StoreWriteError::Draft(DraftError::TooLarge), "large")
        );
        assert!(ok, "{expect}: {err}");
        // Whatever the reason, the message names the rule and never the text.
        assert!(!err.to_string().contains("aaaa"));
    }
    assert_nothing_stored(&mut conn).await;

    // Exactly the cap goes through.
    let turn = CurrentTurn::new("a".repeat(PASTE_MAX_BYTES));
    run(&mut conn, &ctx(Some(&turn), Collection::Granted))
        .await
        .unwrap();
    assert_eq!(draft_count(&mut conn).await, 1);
}

// -- scans -------------------------------------------------------------------------------

fn rust_files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

fn sources() -> Vec<(String, String)> {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    rust_files(&root, &mut files);
    assert!(files.len() > 100, "scan found too few files");
    files
        .into_iter()
        .map(|p| {
            let rel = p
                .strip_prefix(&root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            let text = std::fs::read_to_string(&p).unwrap();
            (rel, text)
        })
        .collect()
}

/// Lines of code, not comments, outside test modules' own scans.
fn code_lines(text: &str) -> impl Iterator<Item = &str> {
    text.lines().filter(|l| !l.trim_start().starts_with("//"))
}

/// No file outside handoff_writer.rs READS `turn_text`: the field may be named in
/// its declaration and in the initialisers of a WriterContext (`turn_text: ...,`),
/// and nowhere else. handoff_writer.rs reads it exactly once, in
/// run_handoff_ingest. So no other operation, and no other module, can see the
/// pasted turn. (The compiler already forbids it: TurnText's accessor is private.
/// This pins that nobody works around it.)
#[test]
fn no_file_but_the_hand_off_writer_reads_turn_text() {
    for (rel, text) in sources() {
        if rel == "conductor/handoff_writer_tests.rs" {
            continue;
        }
        for line in code_lines(&text) {
            if !line.contains("turn_text") {
                continue;
            }
            if rel == "conductor/handoff_writer.rs" {
                continue;
            }
            let t = line.trim();
            assert!(
                t.starts_with("turn_text:") || t.starts_with("pub turn_text:"),
                "{rel}: a read of turn_text outside the hand-off writer: {t}"
            );
        }
    }
    let writer = sources()
        .into_iter()
        .find(|(r, _)| r == "conductor/handoff_writer.rs")
        .unwrap()
        .1;
    let production = writer.split("#[cfg(test)]").next().unwrap();
    let reads = code_lines(production)
        .filter(|l| l.contains(".turn_text"))
        .count();
    assert_eq!(reads, 1, "run_handoff_ingest is the one reader");
}

/// The run's CurrentTurn is created in one place (the chat send), stored in one
/// field and lent in one place. No other file names it or its accessors.
#[test]
fn the_current_turn_has_one_sender_one_field_and_one_lender() {
    let mut setters = 0;
    for (rel, text) in sources() {
        if matches!(
            rel.as_str(),
            "conductor/handoff_writer.rs" | "conductor/handoff_writer_tests.rs"
        ) {
            continue;
        }
        // Production code only: a test module (an attribute directly above `mod`)
        // may hand a test run a turn.
        let production = text.split("#[cfg(test)]\nmod ").next().unwrap();
        for line in code_lines(production) {
            if line.contains("CurrentTurn") {
                assert!(
                    rel == "conductor/lifecycle.rs",
                    "{rel} names CurrentTurn: {line}"
                );
            }
            if line.contains("set_current_turn(") && !line.contains("pub fn set_current_turn") {
                assert_eq!(rel, "commands/messages.rs", "{rel}: {line}");
                setters += 1;
            }
            if line.contains(".lend(") {
                assert_eq!(rel, "conductor/lifecycle.rs", "{rel}: {line}");
            }
        }
    }
    assert_eq!(setters, 1, "only send_message hands a run its turn");
}

/// The turn is never persisted: the rehydrate path never names it, no SQL
/// statement names it, and nothing serialises the run's fields around it.
#[test]
fn the_turn_is_never_persisted_or_restored() {
    let lifecycle = sources()
        .into_iter()
        .find(|(r, _)| r == "conductor/lifecycle.rs")
        .unwrap()
        .1;
    let start = lifecycle
        .find("pub async fn rehydrate_focus_run")
        .expect("rehydrate_focus_run");
    let body = &lifecycle[start..];
    let end = body.find("\n}\n").unwrap();
    assert!(
        !body[..end].contains("current_turn") && !body[..end].contains("set_current_turn"),
        "rehydrate_focus_run must not restore the turn"
    );
    for (rel, text) in sources() {
        if rel.ends_with("handoff_writer_tests.rs") {
            continue;
        }
        for line in code_lines(&text) {
            let in_sql = line.contains("INSERT")
                || line.contains("UPDATE ")
                || line.contains("SELECT")
                || line.contains("CREATE TABLE");
            assert!(
                !(in_sql && (line.contains("current_turn") || line.contains("turn_text"))),
                "{rel}: SQL names the turn: {line}"
            );
        }
    }
    // The schema has no column for it.
    let schema = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("schema");
    for entry in std::fs::read_dir(schema).unwrap() {
        let path = entry.unwrap().path();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            !text.contains("current_turn") && !text.contains("turn_text"),
            "{path:?}"
        );
    }
}
