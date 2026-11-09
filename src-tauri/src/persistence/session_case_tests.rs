// src-tauri/src/persistence/session_case_tests.rs
//
// items.id=881 -- tests of session_store and case_store that need REAL
// encrypted per-persona outputs.db files rather than an in-memory database:
//   - persona isolation (each persona is its own file under its own subkey);
//   - concurrent passes in one chat (nothing serialises runs per chat,
//     decisions.id=939), proving the version compare-and-set and the one-time
//     latch hold when several connections write at once;
//   - the append-only proof for case_strategies.
//
// The unit tests of each store (against an in-memory database) are in
// session_store.rs and case_store.rs.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use sqlx::{Connection, Row, SqliteConnection};
use tokio::sync::Barrier;
use tokio::task::JoinSet;

use super::case_store::{
    append_case_strategy, get_case, list_case_strategies, list_config_deltas_for_entity,
    list_open_cases_for_entity, open_case, record_config_delta, update_case, AppendOutcome,
    CaseState, CaseStoreError, CaseUpdate, NewCase, NewConfigDelta, NewStrategy, OpenOutcome,
};
use super::output_store::open_outputs_db;
use super::session_store::{
    get_session_variable, latch_write_suppressed, list_session_variables, put_session_variable,
    write_mode, LatchError, LatchOutcome, PutOutcome, SessionStoreError, SessionWrite, WriteMode,
};

const KEY_HEX: &str = "deadbeef00112233445566778899aabbccddeeff00112233445566778899aabb";
const OTHER_KEY_HEX: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const CHAT: &str = "chat-1";
const FOCUS: &str = "tech-support";
const ROUTER: &str = "entity-router";

/// Holds ENV_MUTEX and a temporary QR_DATA_ROOT for the life of a test, and
/// restores the variable on drop even if the test panics.
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

async fn open(user: &str, persona: &str) -> SqliteConnection {
    open_outputs_db(user, persona, KEY_HEX)
        .await
        .expect("open persona outputs.db")
}

fn put<'a>(name: &'a str, value: &'a str, run: &'a str, expected: Option<i64>) -> SessionWrite<'a> {
    SessionWrite {
        context_key: CHAT,
        focus_id: FOCUS,
        name,
        value,
        written_by_run: run,
        expected_version: expected,
    }
}

fn case<'a>(case_id: &'a str) -> NewCase<'a> {
    NewCase {
        case_id,
        context_key: CHAT,
        primary_entity_id: ROUTER,
        subject_label: "Router",
        symptoms: "drops",
        opened_by_run: "run-a",
    }
}

fn strat<'a>(case_id: &'a str, run: &'a str, action: &'a str, attempt: i64) -> NewStrategy<'a> {
    NewStrategy {
        case_id,
        source_run_id: run,
        source_action_id: action,
        source_attempt: attempt,
        strategy: "restart the router",
        outcome: "no change",
        user_signal: None,
    }
}

// ---------------------------------------------------------------------------
// Persona isolation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn one_personas_rows_are_invisible_to_another_persona_of_the_same_user() {
    let _root = TestRoot::new().await;
    let user = "iso-user";

    // Persona A fills every store and latches the chat.
    let mut a = open(user, "persona-a").await;
    put_session_variable(&mut a, &put("rung", "3", "run-a", None))
        .await
        .unwrap();
    open_case(&mut a, &case("case-1")).await.unwrap();
    append_case_strategy(&mut a, CHAT, &strat("case-1", "run-a", "diagnose", 1))
        .await
        .unwrap();
    record_config_delta(
        &mut a,
        &NewConfigDelta {
            delta_id: "d1",
            context_key: CHAT,
            session_reference: "s1",
            description: "changed DNS",
            relevance_tags: &["dns"],
            entity_ids: &[ROUTER],
            changed_at: None,
            recorded_by_run: "run-a",
        },
    )
    .await
    .unwrap();
    latch_write_suppressed(&mut a, CHAT, FOCUS, "run-a")
        .await
        .unwrap();
    assert_eq!(
        write_mode(&mut a, CHAT).await.unwrap(),
        WriteMode::Suppressed
    );

    // Persona B, same user, same chat key, same case id, same entity id: nothing of A's is there.
    let mut b = open(user, "persona-b").await;
    assert!(get_session_variable(&mut b, CHAT, FOCUS, "rung")
        .await
        .unwrap()
        .is_none());
    assert!(list_session_variables(&mut b, CHAT, FOCUS)
        .await
        .unwrap()
        .is_empty());
    assert!(get_case(&mut b, "case-1").await.unwrap().is_none());
    assert!(list_open_cases_for_entity(&mut b, ROUTER, None)
        .await
        .unwrap()
        .is_empty());
    assert!(list_case_strategies(&mut b, "case-1")
        .await
        .unwrap()
        .is_empty());
    assert!(list_config_deltas_for_entity(&mut b, ROUTER, Utc::now())
        .await
        .unwrap()
        .is_empty());
    assert_eq!(
        write_mode(&mut b, CHAT).await.unwrap(),
        WriteMode::Open,
        "A's latch is not B's"
    );

    // B writes freely under the very same keys, and creates its own rows
    // instead of finding A's.
    let created = open_case(&mut b, &case("case-1")).await.unwrap();
    assert!(matches!(created, OpenOutcome::Created(_)));
    put_session_variable(&mut b, &put("rung", "1", "run-b", None))
        .await
        .unwrap();
    assert_eq!(
        get_session_variable(&mut b, CHAT, FOCUS, "rung")
            .await
            .unwrap()
            .unwrap()
            .value,
        "1"
    );
    assert_eq!(
        get_session_variable(&mut a, CHAT, FOCUS, "rung")
            .await
            .unwrap()
            .unwrap()
            .value,
        "3",
        "B's write did not touch A"
    );
    // A is still latched and B's writes did not lift it.
    assert!(matches!(
        put_session_variable(&mut a, &put("rung", "4", "run-a", Some(1)))
            .await
            .unwrap_err(),
        SessionStoreError::Latch(LatchError::Suppressed)
    ));
}

#[tokio::test]
async fn the_same_persona_id_under_another_user_is_a_separate_store() {
    let _root = TestRoot::new().await;
    let mut u1 = open("user-one", "persona-x").await;
    let mut u2 = open("user-two", "persona-x").await;
    open_case(&mut u1, &case("case-1")).await.unwrap();
    latch_write_suppressed(&mut u1, CHAT, FOCUS, "r")
        .await
        .unwrap();

    assert!(get_case(&mut u2, "case-1").await.unwrap().is_none());
    assert_eq!(write_mode(&mut u2, CHAT).await.unwrap(), WriteMode::Open);
    assert!(matches!(
        open_case(&mut u2, &case("case-1")).await.unwrap(),
        OpenOutcome::Created(_)
    ));
}

#[tokio::test]
async fn a_personas_file_cannot_be_opened_under_another_personas_or_accounts_key() {
    let _root = TestRoot::new().await;
    let user = "key-user";
    let mut a = open(user, "persona-a").await;
    open_case(&mut a, &case("case-1")).await.unwrap();
    a.close().await.unwrap();

    let personas = crate::persistence::migrations::get_data_root()
        .join("users")
        .join(user)
        .join("personas");
    let a_file = personas.join("persona-a").join("outputs.db");
    assert!(std::fs::metadata(&a_file).unwrap().len() > 0);

    // The subkey is derived per persona (auth::subkeys), so the same bytes
    // placed at persona B's path do not open under B's key.
    std::fs::create_dir_all(personas.join("persona-b")).unwrap();
    std::fs::copy(&a_file, personas.join("persona-b").join("outputs.db")).unwrap();
    let err = open_outputs_db(user, "persona-b", KEY_HEX)
        .await
        .expect_err("persona A's file must not open as persona B")
        .to_string()
        .to_lowercase();
    assert!(
        err.contains("not a database"),
        "refused for the key, not for another reason: {err}"
    );

    // A different data key (another account) fails the same way.
    let err = open_outputs_db(user, "persona-a", OTHER_KEY_HEX)
        .await
        .expect_err("another account's key must not open the file")
        .to_string()
        .to_lowercase();
    assert!(err.contains("not a database"), "{err}");

    // Control: the right persona and key still open it and see the data.
    let mut again = open(user, "persona-a").await;
    assert!(get_case(&mut again, "case-1").await.unwrap().is_some());
}

// ---------------------------------------------------------------------------
// Concurrent passes in one chat
// ---------------------------------------------------------------------------

const PASSES: usize = 8;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn of_several_concurrent_passes_holding_the_same_version_exactly_one_wins() {
    let _root = TestRoot::new().await;
    let user = "cas-user";
    let persona = "persona-a";
    let mut seed = open(user, persona).await;
    put_session_variable(&mut seed, &put("rung", "1", "seed-run", None))
        .await
        .unwrap();

    let barrier = Arc::new(Barrier::new(PASSES));
    let mut set = JoinSet::new();
    for i in 0..PASSES {
        let barrier = barrier.clone();
        set.spawn(async move {
            let mut conn = open(user, persona).await;
            let run = format!("run-{i}");
            let value = format!("value-{i}");
            barrier.wait().await;
            let w = put("rung", &value, &run, Some(1));
            (i, put_session_variable(&mut conn, &w).await)
        });
    }
    let mut winners = Vec::new();
    let mut losers = 0;
    while let Some(res) = set.join_next().await {
        let (i, r) = res.unwrap();
        match r {
            Ok(PutOutcome::Updated { version }) => {
                assert_eq!(version, 2);
                winners.push(i);
            }
            Err(SessionStoreError::VersionConflict {
                current_version: Some(2),
            }) => losers += 1,
            other => panic!("pass {i}: unexpected result {other:?}"),
        }
    }
    assert_eq!(
        winners.len(),
        1,
        "exactly one pass may win the compare-and-set: {winners:?}"
    );
    assert_eq!(losers, PASSES - 1);

    let got = get_session_variable(&mut seed, CHAT, FOCUS, "rung")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(got.version, 2);
    assert_eq!(
        got.value,
        format!("value-{}", winners[0]),
        "the stored value is the winner's"
    );
    assert_eq!(got.written_by_run, format!("run-{}", winners[0]));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_read_modify_write_counter_loses_no_update_across_concurrent_passes() {
    let _root = TestRoot::new().await;
    let user = "counter-user";
    let persona = "persona-a";
    let mut seed = open(user, persona).await;
    put_session_variable(&mut seed, &put("counter", "0", "seed-run", None))
        .await
        .unwrap();

    const INCREMENTS: usize = 5;
    let conflicts = Arc::new(AtomicUsize::new(0));
    let barrier = Arc::new(Barrier::new(PASSES));
    let mut set = JoinSet::new();
    for i in 0..PASSES {
        let (barrier, conflicts) = (barrier.clone(), conflicts.clone());
        set.spawn(async move {
            let mut conn = open(user, persona).await;
            let run = format!("run-{i}");
            barrier.wait().await;
            for _ in 0..INCREMENTS {
                loop {
                    let cur = get_session_variable(&mut conn, CHAT, FOCUS, "counter")
                        .await
                        .unwrap()
                        .unwrap();
                    let next = (cur.value.parse::<u64>().unwrap() + 1).to_string();
                    let w = put("counter", &next, &run, Some(cur.version));
                    match put_session_variable(&mut conn, &w).await {
                        Ok(_) => break,
                        Err(SessionStoreError::VersionConflict { .. }) => {
                            conflicts.fetch_add(1, Ordering::SeqCst);
                        }
                        Err(e) => panic!("unexpected error: {e:?}"),
                    }
                }
            }
        });
    }
    while let Some(res) = set.join_next().await {
        res.unwrap();
    }

    let total = (PASSES * INCREMENTS) as u64;
    let got = get_session_variable(&mut seed, CHAT, FOCUS, "counter")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(got.value, total.to_string(), "no increment was lost");
    assert_eq!(
        got.version as u64,
        total + 1,
        "every increment bumped the version once"
    );
    // The test is only meaningful if the passes really did collide.
    assert!(
        conflicts.load(Ordering::SeqCst) > 0,
        "the passes never collided, so this run proved nothing about the compare-and-set"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_passes_on_one_case_keep_the_log_and_the_version_consistent() {
    let _root = TestRoot::new().await;
    let user = "case-user";
    let persona = "persona-a";
    let mut seed = open(user, persona).await;
    open_case(&mut seed, &case("case-1")).await.unwrap();

    const APPENDS: usize = 6;
    let barrier = Arc::new(Barrier::new(PASSES));
    let rung_wins = Arc::new(AtomicUsize::new(0));
    let mut set = JoinSet::new();
    for i in 0..PASSES {
        let (barrier, rung_wins) = (barrier.clone(), rung_wins.clone());
        set.spawn(async move {
            let mut conn = open(user, persona).await;
            let run = format!("run-{i}");
            barrier.wait().await;
            let mut appended = 0usize;
            let mut duplicates = 0usize;
            for n in 0..APPENDS {
                // Each pass appends its own steps...
                let own = append_case_strategy(
                    &mut conn,
                    CHAT,
                    &strat("case-1", &run, &format!("step-{n}"), 1),
                )
                .await
                .unwrap();
                assert!(matches!(own, AppendOutcome::Appended { .. }));
                appended += 1;
                // ...and every pass also races to append the SAME shared step
                // attempt: exactly one may land.
                let shared = append_case_strategy(
                    &mut conn,
                    CHAT,
                    &strat("case-1", "shared-run", "shared-step", n as i64),
                )
                .await
                .unwrap();
                match shared {
                    AppendOutcome::Appended { .. } => appended += 1,
                    AppendOutcome::Duplicate { .. } => duplicates += 1,
                }
            }
            // And a compare-and-set update of the case's rung, retried until it lands once.
            loop {
                let cur = get_case(&mut conn, "case-1").await.unwrap().unwrap();
                let up = CaseUpdate {
                    current_rung: Some(((cur.current_rung % 5) + 1).min(5)),
                    ..Default::default()
                };
                match update_case(&mut conn, CHAT, "case-1", cur.version, &up).await {
                    Ok(_) => {
                        rung_wins.fetch_add(1, Ordering::SeqCst);
                        break;
                    }
                    Err(CaseStoreError::VersionConflict { .. }) => continue,
                    Err(e) => panic!("unexpected error: {e:?}"),
                }
            }
            (appended, duplicates)
        });
    }
    let (mut appended, mut duplicates) = (0, 0);
    while let Some(res) = set.join_next().await {
        let (a, d) = res.unwrap();
        appended += a;
        duplicates += d;
    }

    // PASSES * APPENDS own entries, plus each of the APPENDS shared attempts exactly once.
    let expected_rows = PASSES * APPENDS + APPENDS;
    assert_eq!(appended, expected_rows);
    assert_eq!(duplicates, PASSES * APPENDS - APPENDS);
    let log = list_case_strategies(&mut seed, "case-1").await.unwrap();
    assert_eq!(
        log.len(),
        expected_rows,
        "the log holds exactly what was acknowledged"
    );
    let mut keys: Vec<_> = log
        .iter()
        .map(|s| {
            (
                s.source_run_id.clone(),
                s.source_action_id.clone(),
                s.source_attempt,
            )
        })
        .collect();
    keys.sort();
    keys.dedup();
    assert_eq!(
        keys.len(),
        expected_rows,
        "no step attempt is recorded twice"
    );

    let case = get_case(&mut seed, "case-1").await.unwrap().unwrap();
    assert_eq!(rung_wins.load(Ordering::SeqCst), PASSES);
    assert_eq!(
        case.version as usize,
        1 + PASSES,
        "every successful update bumped the version once"
    );
    assert_eq!(case.state, CaseState::Open);
}

/// The latch under fire. Writer passes append strategies, update a session
/// variable and update the case in a loop while another pass sets the latch.
///
/// The check is a happens-before one: `latched` is set only AFTER
/// latch_write_suppressed has returned, so a write that STARTED after the flag
/// was set began after the latch had committed and must be refused. Any write
/// acknowledged under that condition is a violation. Afterwards the database
/// must hold exactly the writes that were acknowledged, none of them dated
/// after the latch, and every further write by anyone must be refused.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_latch_holds_against_concurrent_passes() {
    let _root = TestRoot::new().await;
    let user = "latch-user";
    let persona = "persona-a";
    let mut seed = open(user, persona).await;
    open_case(&mut seed, &case("case-1")).await.unwrap();
    put_session_variable(&mut seed, &put("rung", "0", "seed-run", None))
        .await
        .unwrap();

    const WRITERS: usize = 4;
    let latched = Arc::new(AtomicBool::new(false));
    let violations = Arc::new(AtomicUsize::new(0));
    let acknowledged_strategies = Arc::new(AtomicUsize::new(0));
    let acknowledged_puts = Arc::new(AtomicUsize::new(0));
    let barrier = Arc::new(Barrier::new(WRITERS + 1));

    let mut set = JoinSet::new();
    for w in 0..WRITERS {
        let (latched, violations, acked_s, acked_p, barrier) = (
            latched.clone(),
            violations.clone(),
            acknowledged_strategies.clone(),
            acknowledged_puts.clone(),
            barrier.clone(),
        );
        set.spawn(async move {
            let mut conn = open(user, persona).await;
            let run = format!("run-{w}");
            barrier.wait().await;
            let mut refused_in_a_row = 0;
            let mut i = 0usize;
            let deadline = std::time::Instant::now() + Duration::from_secs(30);
            while refused_in_a_row < 5 {
                assert!(
                    std::time::Instant::now() < deadline,
                    "the latch was never observed"
                );
                i += 1;

                let started_latched = latched.load(Ordering::SeqCst);
                let r = append_case_strategy(
                    &mut conn,
                    CHAT,
                    &strat("case-1", &run, &format!("a{i}"), 1),
                )
                .await;
                match r {
                    Ok(AppendOutcome::Appended { .. }) => {
                        acked_s.fetch_add(1, Ordering::SeqCst);
                        if started_latched {
                            violations.fetch_add(1, Ordering::SeqCst);
                        }
                        refused_in_a_row = 0;
                    }
                    Ok(other) => panic!("unexpected {other:?}"),
                    Err(CaseStoreError::Latch(LatchError::Suppressed)) => refused_in_a_row += 1,
                    Err(e) => panic!("append: {e:?}"),
                }

                let started_latched = latched.load(Ordering::SeqCst);
                let cur = get_session_variable(&mut conn, CHAT, FOCUS, "rung")
                    .await
                    .unwrap()
                    .unwrap();
                let value = format!("{run}-{i}");
                let r =
                    put_session_variable(&mut conn, &put("rung", &value, &run, Some(cur.version)))
                        .await;
                match r {
                    Ok(PutOutcome::Updated { .. }) => {
                        acked_p.fetch_add(1, Ordering::SeqCst);
                        if started_latched {
                            violations.fetch_add(1, Ordering::SeqCst);
                        }
                    }
                    Ok(_) | Err(SessionStoreError::VersionConflict { .. }) => {}
                    Err(SessionStoreError::Latch(LatchError::Suppressed)) => refused_in_a_row += 1,
                    Err(e) => panic!("put: {e:?}"),
                }
            }
        });
    }

    let latcher = {
        let (latched, barrier) = (latched.clone(), barrier.clone());
        tokio::spawn(async move {
            let mut conn = open(user, persona).await;
            barrier.wait().await;
            tokio::time::sleep(Duration::from_millis(60)).await;
            let out = latch_write_suppressed(&mut conn, CHAT, FOCUS, "latch-run")
                .await
                .unwrap();
            latched.store(true, Ordering::SeqCst);
            out
        })
    };

    while let Some(res) = set.join_next().await {
        res.unwrap();
    }
    assert_eq!(latcher.await.unwrap(), LatchOutcome::Set);

    assert_eq!(
        violations.load(Ordering::SeqCst),
        0,
        "a write was acknowledged after the latch committed"
    );

    // The database holds exactly what was acknowledged.
    let log = list_case_strategies(&mut seed, "case-1").await.unwrap();
    assert_eq!(log.len(), acknowledged_strategies.load(Ordering::SeqCst));
    assert!(
        !log.is_empty(),
        "no write landed before the latch, so the race was never exercised"
    );
    let rung = get_session_variable(&mut seed, CHAT, FOCUS, "rung")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        rung.version as usize,
        1 + acknowledged_puts.load(Ordering::SeqCst),
        "every acknowledged session write bumped the version once and nothing else did"
    );

    // No acknowledged write is dated after the latch.
    let latch_row =
        sqlx::query("SELECT created_at FROM session_variables WHERE name = 'write_mode'")
            .fetch_one(&mut seed)
            .await
            .unwrap();
    let latch_at = DateTime::parse_from_rfc3339(&latch_row.get::<String, _>("created_at")).unwrap();
    for s in &log {
        let at = DateTime::parse_from_rfc3339(&s.recorded_at).unwrap();
        assert!(
            at <= latch_at,
            "strategy {} recorded at {at} after the latch at {latch_at}",
            s.strategy_id
        );
    }
    let rung_at = DateTime::parse_from_rfc3339(&rung.updated_at).unwrap();
    assert!(rung_at <= latch_at);

    // And now nobody can write, on a fresh connection or an old one.
    let mut fresh = open(user, persona).await;
    assert!(matches!(
        append_case_strategy(&mut fresh, CHAT, &strat("case-1", "late", "late", 1))
            .await
            .unwrap_err(),
        CaseStoreError::Latch(LatchError::Suppressed)
    ));
    assert!(matches!(
        put_session_variable(&mut fresh, &put("rung", "late", "late", Some(rung.version)))
            .await
            .unwrap_err(),
        SessionStoreError::Latch(LatchError::Suppressed)
    ));
    let up = CaseUpdate {
        current_rung: Some(2),
        ..Default::default()
    };
    assert!(matches!(
        update_case(&mut fresh, CHAT, "case-1", 1, &up)
            .await
            .unwrap_err(),
        CaseStoreError::Latch(LatchError::Suppressed)
    ));
    assert_eq!(
        latch_write_suppressed(&mut fresh, CHAT, FOCUS, "again")
            .await
            .unwrap(),
        LatchOutcome::AlreadySet
    );
    let latch_rows: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM session_variables WHERE name = 'write_mode'")
            .fetch_one(&mut fresh)
            .await
            .unwrap();
    assert_eq!(latch_rows, 1, "still one control row");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_latch_calls_set_it_exactly_once() {
    let _root = TestRoot::new().await;
    let user = "latch-race-user";
    let persona = "persona-a";
    drop(open(user, persona).await);

    let barrier = Arc::new(Barrier::new(PASSES));
    let mut set = JoinSet::new();
    for i in 0..PASSES {
        let barrier = barrier.clone();
        set.spawn(async move {
            let mut conn = open(user, persona).await;
            let focus = format!("focus-{i}");
            barrier.wait().await;
            latch_write_suppressed(&mut conn, CHAT, &focus, &format!("run-{i}"))
                .await
                .unwrap()
        });
    }
    let mut sets = 0;
    while let Some(res) = set.join_next().await {
        if res.unwrap() == LatchOutcome::Set {
            sets += 1;
        }
    }
    assert_eq!(sets, 1);
    let mut conn = open(user, persona).await;
    let rows: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM session_variables WHERE name = 'write_mode'")
            .fetch_one(&mut conn)
            .await
            .unwrap();
    assert_eq!(
        rows, 1,
        "one control row for the chat, however many passes latched at once"
    );
}

// ---------------------------------------------------------------------------
// case_strategies is append-only
// ---------------------------------------------------------------------------

fn rust_files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("src dir readable") {
        let path = entry.expect("dir entry").path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// The database itself cannot forbid an UPDATE or DELETE here (decisions.id=939
/// allows no triggers), so the guarantee is the API plus this scan: no Rust
/// source anywhere under src/ may contain a statement that updates, deletes
/// or replaces a case_strategies row, and case_store exposes exactly one
/// writer and one reader for the table. A row leaves the table only through
/// the ON DELETE CASCADE of deleting its case (items.id=888's purge).
///
/// Comments are scanned too: reword rather than exempt. The forbidden phrases
/// are built from pieces so this file does not match itself.
#[test]
fn case_strategies_has_no_update_delete_or_replace_anywhere_in_the_source() {
    let table = "case_strategies";
    let forbidden: Vec<String> = [
        "UPDATE ",
        "DELETE FROM ",
        "REPLACE INTO ",
        "TRUNCATE ",
        "DROP TABLE ",
    ]
    .iter()
    .map(|verb| format!("{verb}{table}").to_lowercase())
    .collect();

    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    rust_files(&src, &mut files);
    assert!(
        files.len() > 50,
        "scan found too few files: {}",
        files.len()
    );

    let mut offenders = Vec::new();
    let mut saw_case_store = false;
    for path in files {
        if path.ends_with("persistence/session_case_tests.rs") {
            continue;
        }
        saw_case_store |= path.ends_with("persistence/case_store.rs");
        let text = std::fs::read_to_string(&path).expect("source readable");
        // Normalise whitespace so a statement split over lines cannot hide.
        let flat = text
            .to_lowercase()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        for f in &forbidden {
            if flat.contains(f.as_str()) {
                offenders.push(format!(
                    "{}: `{f}`",
                    path.strip_prefix(&src).unwrap().display()
                ));
            }
        }
    }
    assert!(saw_case_store, "the scan did not reach case_store.rs");
    assert!(
        offenders.is_empty(),
        "case_strategies must stay append-only: {offenders:?}"
    );

    // Every public function of case_store that mentions strategies.
    let store = std::fs::read_to_string(src.join("persistence/case_store.rs")).unwrap();
    let production = store.split("#[cfg(test)]").next().unwrap();
    let mut strategy_fns: Vec<&str> = production
        .lines()
        .filter_map(|l| l.trim_start().strip_prefix("pub async fn "))
        .filter_map(|rest| rest.split('(').next())
        .filter(|name| name.contains("strateg"))
        .collect();
    strategy_fns.sort_unstable();
    assert_eq!(
        strategy_fns,
        ["append_case_strategy", "list_case_strategies"]
    );
}
