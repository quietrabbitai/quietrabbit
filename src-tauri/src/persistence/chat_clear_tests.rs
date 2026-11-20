// items.id=888 stage 6: clear_chat, the archive hook.

use sqlx::SqliteConnection;

use super::*;
use crate::persistence::output_store::open_outputs_db;
use crate::persistence::session_store::{self, WriteMode};

const KEY_HEX: &str = "c0ffee00112233445566778899aabbccddeeff00112233445566778899aabbcc";
const USER: &str = "u-clear";
const PERSONA: &str = "p-clear";
const CHAT: &str = "chat-gone";
const OTHER: &str = "chat-kept";
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

async fn outputs() -> SqliteConnection {
    open_outputs_db(USER, PERSONA, KEY_HEX).await.unwrap()
}

async fn exec(c: &mut SqliteConnection, sql: &str, binds: &[&str]) {
    let mut q = sqlx::query(sql);
    for b in binds {
        q = q.bind(*b);
    }
    q.execute(&mut *c).await.unwrap();
}

async fn count(c: &mut SqliteConnection, sql: &str, bind: &str) -> i64 {
    sqlx::query_scalar(sql)
        .bind(bind)
        .fetch_one(&mut *c)
        .await
        .unwrap()
}

/// One chat's saved state: a parked run (snapshot, park, pending review), a finished run,
/// a live run, a session variable, the latch, an artifact and a plan approval.
async fn seed_chat(c: &mut SqliteConnection, chat: &str) {
    seed_chat_with(
        c,
        chat,
        &[
            ("parked", "awaiting_user"),
            ("done", "complete"),
            ("live", "running"),
        ],
    )
    .await;
}

/// The chat's saved state with these runs. Every run has one review: pending for the
/// parked run, a finished (clean) one for every other.
async fn seed_chat_with(c: &mut SqliteConnection, chat: &str, runs: &[(&str, &str)]) {
    for (suffix, status) in runs {
        let id = format!("{chat}-{suffix}");
        exec(
            c,
            "INSERT INTO focus_runs (id, focus_id, status, started_at, user_input,
                                     external_input, context_key)
             VALUES (?, ?, ?, 't', 'User: the window', 'User: the window', ?)",
            &[&id, FOCUS, status, chat],
        )
        .await;
        if *suffix != "parked" {
            exec(
                c,
                "INSERT INTO step_reviews (review_id, focus_run_id, action_id, prompt_sha,
                                           prompt_len, outcome, created_at, updated_at)
                 VALUES (?, ?, 'a', ?, 1, 'clean', 't', 't')",
                &[&format!("rev-{id}"), &id, &"c".repeat(64)],
            )
            .await;
        }
    }
    let parked = format!("{chat}-parked");
    if !runs.iter().any(|(s, _)| *s == "parked") {
        exec(
            c,
            "INSERT INTO session_variables (context_key, focus_id, name, value, version,
                                            written_by_run, created_at, updated_at)
             VALUES (?, ?, 'locked_intent', 'billing', 1, 'r', 't', 't')",
            &[chat, FOCUS],
        )
        .await;
        session_store::latch_write_suppressed(c, chat, FOCUS, "r")
            .await
            .unwrap();
        return;
    }
    exec(
        c,
        "INSERT INTO focus_run_snapshots (id, focus_run_id, action_id, phase, checkpoint_hash,
                                          created_at)
         VALUES (?, ?, 'a', 4, 'h', 't')",
        &[&format!("snap-{chat}"), &parked],
    )
    .await;
    for run in [&parked, &format!("{chat}-done")] {
        exec(
            c,
            "INSERT INTO run_parks (run_id, action_id, code, created_at)
             VALUES (?, 'a', 'step_review.pending', 't')",
            &[run],
        )
        .await;
    }
    exec(
        c,
        "INSERT INTO step_reviews (review_id, focus_run_id, action_id, prompt_sha, prompt_len,
                                   outcome, created_at, updated_at)
         VALUES (?, ?, 'a', ?, 1, 'pending', 't', 't')",
        &[&format!("rev-{chat}"), &parked, &"b".repeat(64)],
    )
    .await;
    exec(
        c,
        "INSERT INTO session_variables (context_key, focus_id, name, value, version,
                                        written_by_run, created_at, updated_at)
         VALUES (?, ?, 'locked_intent', 'billing', 1, 'r', 't', 't')",
        &[chat, FOCUS],
    )
    .await;
    // The approval goes through its one writer (it checks the latch, so before it).
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
    session_store::latch_write_suppressed(c, chat, FOCUS, "r")
        .await
        .unwrap();
    exec(
        c,
        "INSERT INTO pass_artifacts (context_key, focus_id, kind, content, written_by_run,
                                     created_at, updated_at)
         VALUES (?, ?, 'content_brief', 'a brief', 'r', 't', 't')",
        &[chat, FOCUS],
    )
    .await;
}

async fn saved_rows(c: &mut SqliteConnection, chat: &str) -> [i64; 4] {
    [
        count(
            c,
            "SELECT COUNT(*) FROM session_variables WHERE context_key = ?",
            chat,
        )
        .await,
        count(
            c,
            "SELECT COUNT(*) FROM pass_artifacts WHERE context_key = ?",
            chat,
        )
        .await,
        count(
            c,
            "SELECT COUNT(*) FROM plan_approvals WHERE context_key = ?",
            chat,
        )
        .await,
        count(
            c,
            "SELECT COUNT(*) FROM run_parks WHERE run_id IN
             (SELECT id FROM focus_runs WHERE context_key = ?)",
            chat,
        )
        .await,
    ]
}

/// The archive hook removes everything QR saved for the chat, the latch included, and
/// cancels its parked runs; another chat and a run with no chat are untouched.
#[tokio::test]
async fn clear_chat_removes_what_the_chat_saved_and_nothing_else() {
    let _root = TestRoot::new().await;
    let mut c = outputs().await;
    seed_chat(&mut c, CHAT).await;
    seed_chat(&mut c, OTHER).await;
    exec(
        &mut c,
        "INSERT INTO focus_runs (id, focus_id, status, started_at, user_input, external_input)
         VALUES ('legacy', ?, 'awaiting_user', 't', 'User: old', 'User: old')",
        &[FOCUS],
    )
    .await;

    let report = clear_chat(USER, PERSONA, KEY_HEX, CHAT).await.unwrap();
    assert_eq!(report.runs_cancelled, 1);
    // The parked run's inputs went with its cancel; the finished and the live run's here.
    assert_eq!(report.inputs_cleared, 2);

    // Everything but the latch: the chat still has a live run (items.id=888 stage 6
    // delta), so the latch stays and only the latch.
    assert_eq!(saved_rows(&mut c, CHAT).await, [1, 0, 0, 0]);
    assert_eq!(
        session_store::write_mode(&mut c, CHAT).await.unwrap(),
        WriteMode::Suppressed,
        "a live run keeps the latch"
    );
    for (suffix, want) in [
        ("parked", "cancelled"),
        ("done", "complete"),
        ("live", "running"),
    ] {
        let id = format!("{CHAT}-{suffix}");
        let (status, input, external): (String, Option<String>, Option<String>) = sqlx::query_as(
            "SELECT status, user_input, external_input FROM focus_runs WHERE id = ?",
        )
        .bind(&id)
        .fetch_one(&mut c)
        .await
        .unwrap();
        assert_eq!(status, want, "{id}");
        assert_eq!((input, external), (None, None), "{id}");
    }
    let parked = format!("{CHAT}-parked");
    assert_eq!(
        count(
            &mut c,
            "SELECT COUNT(*) FROM focus_run_snapshots WHERE focus_run_id = ?",
            &parked
        )
        .await,
        0
    );
    let reason: Option<String> = sqlx::query_scalar(
        "SELECT json_extract(notes, '$.cancel_reason') FROM focus_runs WHERE id = ?",
    )
    .bind(&parked)
    .fetch_one(&mut c)
    .await
    .unwrap();
    assert_eq!(reason.as_deref(), Some(CHAT_ARCHIVED));
    // The reviews of the chat's runs that ended (the parked run, now cancelled, and the
    // finished one) are deleted; the live run keeps its own (review_prompt reuses it).
    for (run, want) in [
        (parked.clone(), 0),
        (format!("{CHAT}-done"), 0),
        (format!("{CHAT}-live"), 1),
    ] {
        assert_eq!(
            count(
                &mut c,
                "SELECT COUNT(*) FROM step_reviews WHERE focus_run_id = ?",
                &run
            )
            .await,
            want,
            "{run}"
        );
    }

    // The other chat and the run with no chat are as they were.
    assert_eq!(saved_rows(&mut c, OTHER).await, [2, 1, 1, 2]);
    assert_eq!(
        count(
            &mut c,
            "SELECT COUNT(*) FROM step_reviews WHERE focus_run_id IN
             (SELECT id FROM focus_runs WHERE context_key = ?)",
            OTHER
        )
        .await,
        3
    );
    assert_eq!(
        session_store::write_mode(&mut c, OTHER).await.unwrap(),
        WriteMode::Suppressed
    );
    let other_parked: (String, Option<String>) =
        sqlx::query_as("SELECT status, user_input FROM focus_runs WHERE id = ?")
            .bind(format!("{OTHER}-parked"))
            .fetch_one(&mut c)
            .await
            .unwrap();
    assert_eq!(
        other_parked,
        (
            "awaiting_user".to_owned(),
            Some("User: the window".to_owned())
        )
    );
    let legacy: (String, Option<String>) =
        sqlx::query_as("SELECT status, user_input FROM focus_runs WHERE id = 'legacy'")
            .fetch_one(&mut c)
            .await
            .unwrap();
    assert_eq!(
        legacy,
        ("awaiting_user".to_owned(), Some("User: old".to_owned()))
    );

    assert_eq!(report.reviews_deleted, 2);
    assert!(report.latch_kept);

    // A second clear has nothing to do (the live run still keeps the latch).
    assert_eq!(
        clear_chat(USER, PERSONA, KEY_HEX, CHAT).await.unwrap(),
        ChatCleared {
            latch_kept: true,
            ..ChatCleared::default()
        }
    );
}

/// items.id=888 stage 6 delta: the latch goes with the chat only when no run of the chat
/// is still in a non-terminal status after the parked runs were cancelled. A live
/// (running) or an awaiting_feedback run keeps it, and every other session variable still
/// goes; a chat whose runs all ended (a parked run counts: it is cancelled first) loses it.
#[tokio::test]
async fn the_latch_stays_while_a_run_of_the_chat_has_not_ended() {
    let _root = TestRoot::new().await;
    let mut c = outputs().await;
    for (chat, status) in [
        ("chat-running", "running"),
        ("chat-feedback", "awaiting_feedback"),
    ] {
        seed_chat_with(&mut c, chat, &[("open", status), ("done", "complete")]).await;
        clear_chat(USER, PERSONA, KEY_HEX, chat).await.unwrap();
        assert_eq!(
            session_store::write_mode(&mut c, chat).await.unwrap(),
            WriteMode::Suppressed,
            "{status} keeps the latch"
        );
        assert_eq!(
            count(
                &mut c,
                "SELECT COUNT(*) FROM session_variables WHERE context_key = ? AND name <> 'write_mode'",
                chat
            )
            .await,
            0,
            "{status}: the other session variables go"
        );
    }
    seed_chat_with(
        &mut c,
        "chat-ended",
        &[
            ("done", "complete"),
            ("bad", "failed"),
            ("stopped", "cancelled"),
        ],
    )
    .await;
    clear_chat(USER, PERSONA, KEY_HEX, "chat-ended")
        .await
        .unwrap();
    assert_eq!(
        session_store::write_mode(&mut c, "chat-ended")
            .await
            .unwrap(),
        WriteMode::Open,
        "only finished runs: the latch goes"
    );
    assert_eq!(
        count(
            &mut c,
            "SELECT COUNT(*) FROM session_variables WHERE context_key = ?",
            "chat-ended"
        )
        .await,
        0
    );
}

/// items.id=888 stage 6 delta: step_review_store has exactly one delete, scoped to the
/// reviews of a chat's ENDED runs, and persistence::chat_clear is its only caller.
#[test]
fn step_reviews_have_one_delete_and_the_archive_hook_is_its_only_caller() {
    let store = include_str!("step_review_store.rs");
    let code = store.split("#[cfg(test)]").next().unwrap();
    assert_eq!(code.matches("DELETE FROM").count(), 1);
    let at = code
        .find("pub(crate) async fn delete_reviews_of_ended_chat_runs_in_tx(")
        .expect("the one delete");
    let body = &code[at..];
    let body = &body[..body.find("\n}\n").unwrap()];
    assert!(body.contains("DELETE FROM step_reviews"));
    assert!(body.contains("f.context_key = ?"));
    assert!(body.contains("f.status IN ('complete', 'failed', 'cancelled')"));

    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut callers = Vec::new();
    let mut stack = vec![root.clone()];
    while let Some(dir) = stack.pop() {
        for e in std::fs::read_dir(dir).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                stack.push(p);
                continue;
            }
            if p.extension().is_none_or(|x| x != "rs") || p.to_string_lossy().ends_with("_tests.rs")
            {
                continue;
            }
            let text = std::fs::read_to_string(&p).unwrap();
            let code = text.split("#[cfg(test)]\nmod tests {").next().unwrap();
            if code.contains("delete_reviews_of_ended_chat_runs_in_tx(")
                && !p.ends_with("step_review_store.rs")
            {
                callers.push(p.strip_prefix(&root).unwrap().display().to_string());
            }
        }
    }
    assert_eq!(callers, ["persistence/chat_clear.rs"]);
}

/// The latch is removed in exactly one place: the archive hook's session-store delete.
/// Every other delete of session_variables names the rows it removes and so can never
/// reach the write_mode row.
#[test]
fn the_latch_is_removed_only_by_the_archive_hook() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut unscoped = Vec::new();
    let mut stack = vec![root.clone()];
    while let Some(dir) = stack.pop() {
        for e in std::fs::read_dir(dir).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                stack.push(p);
                continue;
            }
            let is_test_file = p.to_string_lossy().ends_with("_tests.rs");
            if p.extension().is_none_or(|x| x != "rs") || is_test_file {
                continue;
            }
            let text = std::fs::read_to_string(&p).unwrap();
            let code = text.split("#[cfg(test)]\nmod tests {").next().unwrap();
            let flat = code.split_whitespace().collect::<Vec<_>>().join(" ");
            for (i, _) in flat.match_indices("DELETE FROM session_variables") {
                let stmt = &flat[i..(i + 160).min(flat.len())];
                let stmt = stmt.split('"').next().unwrap();
                if !stmt.contains("name") {
                    unscoped.push(format!(
                        "{}: {stmt}",
                        p.strip_prefix(&root).unwrap().display()
                    ));
                }
            }
        }
    }
    assert_eq!(unscoped.len(), 1, "{unscoped:?}");
    assert!(
        unscoped[0].starts_with("persistence/session_store.rs"),
        "{unscoped:?}"
    );
    let store = include_str!("session_store.rs");
    let code = store.split("#[cfg(test)]").next().unwrap();
    let at = code
        .find("pub(crate) async fn clear_chat_in_tx(")
        .expect("the one unscoped delete is clear_chat_in_tx");
    let body = &code[at..];
    let body = &body[..body.find("\n}\n").unwrap()];
    assert!(body.contains("DELETE FROM session_variables WHERE context_key = ?"));
}

/// archive_chat calls the hook after the chat is archived, best effort (a failure is
/// logged and the archive still succeeds), and clears the memory home too.
#[test]
fn archive_chat_calls_the_hook_best_effort() {
    let chats = include_str!("../commands/chats.rs");
    let code = chats.split("#[cfg(test)]").next().unwrap();
    let start = code.find("pub async fn archive_chat(").unwrap();
    let body = &code[start..];
    let body = &body[..body.find("\n}\n").unwrap()];
    let archived = body.find("chat_store::archive_chat(").unwrap();
    let hook = body
        .find("crate::persistence::chat_clear::clear_chat(")
        .expect("the hook");
    assert!(archived < hook);
    assert!(
        body[hook..].contains("log::warn!"),
        "a failure is logged, not returned"
    );
    assert!(body.contains("plan_memory.clear_chat(&user_id, &persona_id, &chat_id);"));
}
