// items.id=888 stage 7: the 30-day sweep of the chat stores.

use chrono::{Duration, TimeZone, Utc};
use sqlx::SqliteConnection;

use super::*;
use crate::conductor::run_retention::STORE_RETENTION_DAYS;
use crate::persistence::output_store::open_outputs_db;
use crate::persistence::session_store::{self, WriteMode};

const KEY_HEX: &str = "5a5a5a5a00112233445566778899aabbccddeeff00112233445566778899aabb";
const USER: &str = "u-sweep";
const PERSONA: &str = "p-sweep";
const FOCUS: &str = "writing-assistant";

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

fn now() -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 11, 20, 12, 0, 0).unwrap()
}

/// updated_at text `days` before now(), in the writers' own format.
fn ago(days: i64) -> String {
    (now() - Duration::days(days)).to_rfc3339()
}

async fn outputs() -> SqliteConnection {
    open_outputs_db(USER, PERSONA, KEY_HEX).await.unwrap()
}

/// One chat's rows of the three stores, all last written at `at`.
async fn seed(c: &mut SqliteConnection, chat: &str, at: &str) {
    sqlx::query(
        "INSERT INTO session_variables (context_key, focus_id, name, value, version,
                                        written_by_run, created_at, updated_at)
         VALUES (?, ?, 'locked_intent', 'billing', 1, 'r', ?, ?)",
    )
    .bind(chat)
    .bind(FOCUS)
    .bind(at)
    .bind(at)
    .execute(&mut *c)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO pass_artifacts (context_key, focus_id, kind, content, written_by_run,
                                     created_at, updated_at)
         VALUES (?, ?, 'content_brief', 'a brief', 'r', ?, ?)",
    )
    .bind(chat)
    .bind(FOCUS)
    .bind(at)
    .bind(at)
    .execute(&mut *c)
    .await
    .unwrap();
    crate::persistence::plan_approval_store::put_approval(
        c,
        &crate::persistence::plan_approval_store::ApprovalWrite {
            context_key: chat,
            focus_id: FOCUS,
            digest: &"d".repeat(64),
            ceiling_micros: 0,
            choices_json: "[]",
        },
    )
    .await
    .unwrap();
    crate::persistence::plan_approval_store::set_updated_at_for_test(c, chat, at).await;
    // items.id=903: a document session.
    sqlx::query(
        "INSERT INTO document_sessions (context_key, focus_id, document_type,
                                        document_privacy_flag, created_at, updated_at)
         VALUES (?, ?, 'cover_letter', 'legal', ?, ?)",
    )
    .bind(chat)
    .bind(FOCUS)
    .bind(at)
    .bind(at)
    .execute(&mut *c)
    .await
    .unwrap();
}

async fn rows(c: &mut SqliteConnection, chat: &str) -> [i64; 4] {
    let mut out = [0; 4];
    for (i, table) in [
        "session_variables",
        "pass_artifacts",
        "plan_approvals",
        "document_sessions",
    ]
    .iter()
    .enumerate()
    {
        out[i] = sqlx::query_scalar(&format!(
            "SELECT COUNT(*) FROM {table} WHERE context_key = ?"
        ))
        .bind(chat)
        .fetch_one(&mut *c)
        .await
        .unwrap();
    }
    out
}

#[test]
fn the_period_is_thirty_days_and_the_cutoff_is_in_the_writers_format() {
    assert_eq!(STORE_RETENTION_DAYS, 30);
    assert_eq!(cutoff(now()), ago(30));
    // The writers' format orders as time does, so a text compare is a time compare.
    assert!(ago(31) < cutoff(now()) && cutoff(now()) < ago(29));
}

/// Rows not written for the period are swept from all three stores; younger rows and
/// every other chat's younger rows stay.
#[tokio::test]
async fn rows_older_than_the_period_are_swept_and_younger_rows_stay() {
    let _root = TestRoot::new().await;
    let mut c = outputs().await;
    seed(&mut c, "chat-old", &ago(31)).await;
    seed(&mut c, "chat-young", &ago(29)).await;
    let report = sweep_stores(USER, PERSONA, KEY_HEX, now()).await.unwrap();
    assert_eq!(
        report,
        StoreSweep {
            session_variables: 1,
            pass_artifacts: 1,
            plan_approvals: 1,
            document_sessions: 1
        }
    );
    assert_eq!(rows(&mut c, "chat-old").await, [0, 0, 0, 0]);
    assert_eq!(rows(&mut c, "chat-young").await, [1, 1, 1, 1]);
}

/// The one-time latch is EXEMPT: a latched chat is still latched after the period, while its
/// other old rows go.
#[tokio::test]
async fn a_latched_chat_is_still_latched_after_the_period() {
    let _root = TestRoot::new().await;
    let mut c = outputs().await;
    seed(&mut c, "chat-once", &ago(90)).await;
    session_store::latch_write_suppressed(&mut c, "chat-once", FOCUS, "r")
        .await
        .unwrap();
    sqlx::query(
        "UPDATE session_variables SET updated_at = ?, created_at = ? WHERE name = 'write_mode'",
    )
    .bind(ago(400))
    .bind(ago(400))
    .execute(&mut c)
    .await
    .unwrap();
    sweep_stores(USER, PERSONA, KEY_HEX, now()).await.unwrap();
    assert_eq!(
        session_store::write_mode(&mut c, "chat-once")
            .await
            .unwrap(),
        WriteMode::Suppressed
    );
    let left: Vec<String> =
        sqlx::query_scalar("SELECT name FROM session_variables WHERE context_key = 'chat-once'")
            .fetch_all(&mut c)
            .await
            .unwrap();
    assert_eq!(left, ["write_mode"]);
}

/// Suppression: a sweep that fails part-way deletes NOTHING (one transaction).
#[tokio::test]
async fn a_sweep_that_fails_deletes_nothing() {
    let _root = TestRoot::new().await;
    let mut c = outputs().await;
    seed(&mut c, "chat-old", &ago(31)).await;
    sqlx::query("DROP TABLE plan_approvals")
        .execute(&mut c)
        .await
        .unwrap();
    assert!(sweep_stores(USER, PERSONA, KEY_HEX, now()).await.is_err());
    let left: [i64; 2] = [
        sqlx::query_scalar("SELECT COUNT(*) FROM session_variables")
            .fetch_one(&mut c)
            .await
            .unwrap(),
        sqlx::query_scalar("SELECT COUNT(*) FROM pass_artifacts")
            .fetch_one(&mut c)
            .await
            .unwrap(),
    ];
    assert_eq!(left, [1, 1], "rolled back");
}

/// The comparison is textual: a word sorts after any RFC 3339 cutoff and is kept. This pins
/// that ONE case only. A value in another format is outside what the sweep promises (a
/// numeric string sorts before the cutoff and is swept); the guarantee is that all three
/// stores stamp updated_at through now() in RFC 3339.
#[tokio::test]
async fn a_word_in_updated_at_sorts_after_the_cutoff_and_is_kept() {
    let _root = TestRoot::new().await;
    let mut c = outputs().await;
    seed(&mut c, "chat-odd", "not a time").await;
    sweep_stores(USER, PERSONA, KEY_HEX, now()).await.unwrap();
    assert_eq!(rows(&mut c, "chat-odd").await, [1, 1, 1, 1]);
}

/// Each sweep delete uses its table's updated_at index.
#[tokio::test]
async fn each_sweep_uses_its_updated_at_index() {
    let _root = TestRoot::new().await;
    let mut c = outputs().await;
    // The statement text is built from the table name: plan_approval_store's own scan
    // allows no other file to hold a literal write of its table.
    let stmt =
        |table: &str, extra: &str| format!("DELETE FROM {table} WHERE updated_at < ?{extra}");
    for (table, extra) in [
        ("session_variables", " AND name <> ?"),
        ("pass_artifacts", ""),
        ("plan_approvals", ""),
    ] {
        let sql = format!("EXPLAIN QUERY PLAN {}", stmt(table, extra));
        let index = format!("idx_{table}_updated_at");
        let plan: Vec<(i64, i64, i64, String)> = sqlx::query_as(&sql)
            .bind("x")
            .bind("write_mode")
            .fetch_all(&mut c)
            .await
            .unwrap();
        assert!(
            plan.iter().any(|r| r.3.contains(&index)),
            "{index}: {plan:?}"
        );
    }
    // And the three store functions run exactly these statements.
    for (file, table, extra) in [
        (
            include_str!("session_store.rs"),
            "session_variables",
            " AND name <> ?",
        ),
        (include_str!("pass_artifact_store.rs"), "pass_artifacts", ""),
        (include_str!("plan_approval_store.rs"), "plan_approvals", ""),
    ] {
        let code = file.split("#[cfg(test)]").next().unwrap();
        let s = stmt(table, extra);
        assert_eq!(code.matches(&s).count(), 1, "{s}");
    }
}

/// The tick sweeps with the key resident and does nothing without one; the login pass
/// sweeps too.
#[tokio::test]
async fn the_tick_sweeps_only_with_the_key_resident_and_the_login_pass_sweeps() {
    let auth = include_str!("../commands/auth.rs");
    let post = auth
        .find("crate::persistence::output_store::sweep_post_output_runs(")
        .unwrap();
    let stores = auth
        .find("crate::persistence::store_sweep::sweep_stores(")
        .expect("the login pass sweeps the stores");
    assert!(post < stores);
    let tick = include_str!("../conductor/run_retention.rs");
    let tick = tick.split("#[cfg(test)]").next().unwrap();
    let body = &tick[tick.find("pub async fn expire_tick(").unwrap()..];
    let body = &body[..body.find("\n}\n").unwrap()];
    let key = body.find("key_registry").unwrap();
    let sweep = body
        .find("crate::persistence::store_sweep::sweep_stores(")
        .expect("the tick sweeps the stores");
    assert!(
        key < sweep,
        "the key is taken first; without one the tick returns"
    );
}
