// src-tauri/src/conductor/run_retention.rs
//
// items.id=681 (decisions.id=865): retention of a PARKED run's snapshots.
//
// A snapshot (focus_run_snapshots) holds step outputs and disclosure buffers
// with the values approved for the step, encrypted at rest in the persona's
// outputs.db. Only a run parked in awaiting_user or awaiting_schedule keeps
// its snapshots (complete, cancelled and failed delete them at once). This
// module owns WHEN such a snapshot expires:
//   awaiting_user:     checkpoint time + AWAITING_USER_RETENTION_DAYS; every
//                      new checkpoint writes a new row, so it refreshes.
//   awaiting_schedule: the step's scheduled_for + AWAITING_SCHEDULE_RETENTION_DAYS.
//   a periodic checkpoint of a RUNNING run: no purge_after (the run is not
//   parked; the next park writes one).
// The pure functions take the time as an argument, so tests inject a clock.
//
// THE SWEEP (stage 2). persistence::output_store::expire_parked_run decides and
// acts in one transaction; this module holds the pure expiry rule, the parked
// status set and the tick. The parked set is awaiting_user, awaiting_schedule and
// paused: demote_interrupted_runs turns a crashed run into `paused`, and its
// latest snapshot is a periodic one with NO purge_after, so for a snapshot with
// no purge_after the expiry is its created_at + AWAITING_USER_RETENTION_DAYS.
// Expiry is enforced only while the owner's key is resident (decisions.id=847):
// the tick takes the key from the registry and does nothing without one, and the
// login pass runs inside the login recovery task. A run with no snapshot at all
// expires at started_at + AWAITING_USER_RETENTION_DAYS (it cannot resume and still
// holds user_input; an unparseable started_at is never expired).

use chrono::{DateTime, Duration, SecondsFormat, Utc};

use crate::auth::registry::{key_hex, KeyRegistry};
use crate::persistence::output_store::expire_parked_runs;
use crate::persistence::persona_store::list_persona_ids_for_user;

/// The plain message a run carries when its snapshot expired while it waited.
pub const EXPIRED_WHILE_WAITING: &str = "expired while waiting";

/// Statuses a sweep may expire: runs waiting for the user, for a schedule, or
/// paused by a crash recovery.
pub const PARKED_STATUSES: &[&str] = &["awaiting_user", "awaiting_schedule", "paused"];

/// `'a','b','c'` for an SQL IN list (the statuses are fixed words, never input).
pub fn parked_status_list_sql() -> String {
    PARKED_STATUSES
        .iter()
        .map(|s| format!("'{s}'"))
        .collect::<Vec<_>>()
        .join(",")
}

/// Days a run parked for the user keeps its snapshot after its latest checkpoint.
pub const AWAITING_USER_RETENTION_DAYS: i64 = 30;
/// Days a run parked for a schedule keeps its snapshot after its scheduled time.
pub const AWAITING_SCHEDULE_RETENTION_DAYS: i64 = 30;

/// What a checkpoint is being written for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckpointPark {
    /// A periodic checkpoint of a run that keeps running: no purge_after.
    Running,
    /// The run parks at awaiting_user.
    AwaitingUser,
    /// The run parks at awaiting_schedule until `scheduled_for`.
    AwaitingSchedule(DateTime<Utc>),
}

/// One fixed text format for every purge_after value, so a text comparison in
/// SQL is a time comparison (whole seconds, UTC, `Z`).
pub fn format_purge_after(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// The purge_after to store for a checkpoint written at `now`, or None.
pub fn purge_after_for(park: CheckpointPark, now: DateTime<Utc>) -> Option<String> {
    match park {
        CheckpointPark::Running => None,
        CheckpointPark::AwaitingUser => Some(format_purge_after(
            now + Duration::days(AWAITING_USER_RETENTION_DAYS),
        )),
        CheckpointPark::AwaitingSchedule(scheduled_for) => Some(format_purge_after(
            scheduled_for + Duration::days(AWAITING_SCHEDULE_RETENTION_DAYS),
        )),
    }
}

/// Is a snapshot past its retention at `now`? `purge_after` when it has one, else
/// `created_at` plus AWAITING_USER_RETENTION_DAYS (a periodic checkpoint left by
/// a crash). A value that does not parse counts as absent, and an unparseable
/// created_at is NOT expired (nothing is deleted on a guess).
pub fn snapshot_expired(purge_after: Option<&str>, created_at: &str, now: DateTime<Utc>) -> bool {
    let parse = |t: &str| {
        DateTime::parse_from_rfc3339(t)
            .ok()
            .map(|d| d.with_timezone(&Utc))
    };
    let expiry = purge_after
        .and_then(parse)
        .or_else(|| parse(created_at).map(|c| c + Duration::days(AWAITING_USER_RETENTION_DAYS)));
    match expiry {
        Some(e) => now > e,
        None => false,
    }
}

/// A parked run with NO snapshot cannot resume and still holds user_input:
/// it expires at started_at + AWAITING_USER_RETENTION_DAYS. An unparseable
/// started_at is never expired.
pub fn run_expired_without_snapshot(started_at: &str, now: DateTime<Utc>) -> bool {
    match DateTime::parse_from_rfc3339(started_at) {
        Ok(s) => now > s.with_timezone(&Utc) + Duration::days(AWAITING_USER_RETENTION_DAYS),
        Err(_) => false,
    }
}

/// One expiry tick. Does nothing with no key resident, and nothing while a
/// rotation refuses the lease. `now` is the clock (injected by tests);
/// `on_expired(persona_id, run_id)` is told of each expired run so the caller
/// can emit the run-status event. Lease kind: Sweep (short, not visible work, so
/// it neither defers demote_interrupted_runs nor counts as a run to wait for).
pub async fn expire_tick(
    pool: &sqlx::SqlitePool,
    key_registry: &KeyRegistry,
    now: DateTime<Utc>,
    mut on_expired: impl FnMut(&str, &str),
) {
    let Some((user_id, key_hex_str)) = key_registry
        .with_key(|k| (k.user_id.clone(), key_hex(&k.data_key)))
        .await
    else {
        return;
    };
    let Ok(_sweep_lease) =
        crate::auth::writer_gate::acquire(&user_id, crate::auth::writer_gate::Kind::Sweep)
    else {
        return;
    };
    let persona_ids = match list_persona_ids_for_user(pool, &user_id).await {
        Ok(p) => p,
        Err(e) => {
            log::warn!("run_retention: could not list personas for resident user: {e}");
            return;
        }
    };
    for persona_id in persona_ids {
        match expire_parked_runs(&user_id, &persona_id, &key_hex_str, now).await {
            Ok(ids) => {
                for id in &ids {
                    on_expired(&persona_id, id);
                }
            }
            Err(e) => log::debug!("run_retention: persona '{persona_id}' not swept: {e}"),
        }
    }
}

/// The supervised loop's tick: expire, then emit "run-status-update" for each
/// expired run so the UI updates.
pub async fn run_expiry_tick(
    pool: &sqlx::SqlitePool,
    key_registry: &KeyRegistry,
    app_handle: Option<tauri::AppHandle<tauri::Wry>>,
) {
    expire_tick(pool, key_registry, Utc::now(), |_persona, run_id| {
        if let Some(h) = &app_handle {
            emit_expired(h, run_id);
        }
    })
    .await;
}

fn emit_expired(handle: &tauri::AppHandle<tauri::Wry>, run_id: &str) {
    emit_cancelled(handle, run_id, EXPIRED_WHILE_WAITING);
}

/// Emit "run-status-update" for a run cancelled while parked, carrying the plain
/// message in step_display_name (focus_runs has no message column).
pub fn emit_cancelled(handle: &tauri::AppHandle<tauri::Wry>, run_id: &str, message: &str) {
    use tauri::Emitter;
    let payload = crate::conductor::lifecycle::RunStatusPayload {
        focus_run_id: run_id.to_owned(),
        status: "cancelled".to_owned(),
        current_step: 0,
        total_steps: 0,
        step_display_name: Some(message.to_owned()),
        step_content: None,
        crisis_resource_block: None,
        provenance_omissions: Vec::new(),
        personal_flagged: None,
        audit_unavailable: None,
    };
    if let Err(e) = handle.emit("run-status-update", &payload) {
        log::warn!("run_retention: emit run-status-update failed: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn t(y: i32, m: u32, d: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, m, d, 12, 0, 0).unwrap()
    }

    #[test]
    fn the_two_retention_periods_are_thirty_days() {
        assert_eq!(AWAITING_USER_RETENTION_DAYS, 30);
        assert_eq!(AWAITING_SCHEDULE_RETENTION_DAYS, 30);
    }

    #[test]
    fn awaiting_user_is_now_plus_thirty_days() {
        assert_eq!(
            purge_after_for(CheckpointPark::AwaitingUser, t(2026, 11, 19)).as_deref(),
            Some("2026-12-19T12:00:00Z")
        );
    }

    #[test]
    fn awaiting_schedule_ignores_now_and_uses_scheduled_for_plus_thirty_days() {
        let scheduled = t(2027, 3, 1);
        let a = purge_after_for(CheckpointPark::AwaitingSchedule(scheduled), t(2026, 11, 19));
        let b = purge_after_for(CheckpointPark::AwaitingSchedule(scheduled), t(2027, 1, 1));
        assert_eq!(a.as_deref(), Some("2027-03-31T12:00:00Z"));
        assert_eq!(a, b);
    }

    #[test]
    fn a_running_checkpoint_has_no_purge_after() {
        assert_eq!(
            purge_after_for(CheckpointPark::Running, t(2026, 11, 19)),
            None
        );
    }

    // ----- items.id=681 stage 2: the expiry rule and the sweep --------------------

    use crate::auth::registry::{KeyRegistry, UnlockedKey};
    use crate::persistence::output_store::{expire_parked_run, ExpireOutcome};
    use sqlx::Row;

    const KEY_HEX: &str = "3344556677889900334455667788990033445566778899003344556677889900";

    fn data_key() -> [u8; crate::auth::kdf::MASTER_KEY_LEN] {
        let mut k = [0u8; crate::auth::kdf::MASTER_KEY_LEN];
        for (i, b) in k.iter_mut().enumerate() {
            *b = u8::from_str_radix(&KEY_HEX[i * 2..i * 2 + 2], 16).unwrap();
        }
        k
    }

    async fn setup(user_id: &str, persona_id: &str) -> sqlx::SqlitePool {
        crate::persistence::migrations::migrate_shared_db()
            .await
            .unwrap();
        let pool =
            sqlx::SqlitePool::connect_with(crate::providers::utils::connect_options_unencrypted(
                &crate::providers::utils::db_path_shared(),
            ))
            .await
            .unwrap();
        crate::auth::user_store::create_user(
            &pool,
            user_id,
            "Retention Test User",
            "user",
            false,
            &[0u8; crate::auth::kdf::SALT_LEN],
            crate::auth::kdf::DEFAULT_ARGON2_MEMORY_KIB,
            crate::auth::kdf::DEFAULT_ARGON2_ITERATIONS,
            crate::auth::kdf::DEFAULT_ARGON2_PARALLELISM,
            &[0u8; 32],
        )
        .await
        .unwrap();
        crate::persistence::persona_store::create_persona_anchor_for_test(
            &pool, persona_id, user_id,
        )
        .await
        .unwrap();
        pool
    }

    async fn conn(user_id: &str, persona_id: &str) -> sqlx::SqliteConnection {
        crate::persistence::output_store::open_outputs_db(user_id, persona_id, KEY_HEX)
            .await
            .unwrap()
    }

    async fn add_run(c: &mut sqlx::SqliteConnection, id: &str, status: &str, started: &str) {
        sqlx::query(
            "INSERT INTO focus_runs
             (id, focus_id, status, is_fast_lane, is_quick_ask, topic_id, user_input,
              external_input, started_at)
             VALUES (?, 'quick-ask', ?, 0, 1, NULL, 'window text', 'user turns', ?)",
        )
        .bind(id)
        .bind(status)
        .bind(started)
        .execute(c)
        .await
        .unwrap();
    }

    async fn add_snapshot(
        c: &mut sqlx::SqliteConnection,
        run_id: &str,
        purge_after: Option<&str>,
        created_at: &str,
    ) {
        sqlx::query(
            "INSERT INTO focus_run_snapshots
             (id, focus_run_id, action_id, phase, checkpoint_hash, purge_after, created_at)
             VALUES (?, ?, 'a', 4, 'h', ?, ?)",
        )
        .bind(uuid::Uuid::new_v4().to_string())
        .bind(run_id)
        .bind(purge_after)
        .bind(created_at)
        .execute(c)
        .await
        .unwrap();
    }

    async fn state(c: &mut sqlx::SqliteConnection, id: &str) -> (String, Option<String>, i64) {
        let r = sqlx::query(
            "SELECT status, user_input,
                    (SELECT COUNT(*) FROM focus_run_snapshots WHERE focus_run_id = ?) AS n
             FROM focus_runs WHERE id = ?",
        )
        .bind(id)
        .bind(id)
        .fetch_one(c)
        .await
        .unwrap();
        (r.get("status"), r.get("user_input"), r.get("n"))
    }

    /// Lock the env, point the data root at a fresh directory, run the body.
    macro_rules! with_root {
        ($body:expr) => {{
            let _lock = crate::test_support::ENV_MUTEX.lock().await;
            let saved = std::env::var("QR_DATA_ROOT").ok();
            let dir = tempfile::tempdir().unwrap();
            std::env::set_var("QR_DATA_ROOT", dir.path());
            $body;
            match saved {
                Some(v) => std::env::set_var("QR_DATA_ROOT", v),
                None => std::env::remove_var("QR_DATA_ROOT"),
            }
        }};
    }

    #[test]
    fn the_expiry_rule_uses_purge_after_else_created_at_plus_thirty_days() {
        let pa = "2026-12-01T12:00:00Z";
        assert!(!snapshot_expired(
            Some(pa),
            "2026-01-01T00:00:00+00:00",
            t(2026, 12, 1)
        ));
        assert!(snapshot_expired(
            Some(pa),
            "2026-11-30T00:00:00+00:00",
            t(2026, 12, 1) + Duration::seconds(1)
        ));
        // No purge_after (a periodic snapshot a crash left): created_at + 30 days.
        let c = "2026-11-01T12:00:00.123456789+00:00";
        assert!(!snapshot_expired(None, c, t(2026, 12, 1)));
        assert!(snapshot_expired(
            None,
            c,
            t(2026, 12, 1) + Duration::seconds(1)
        ));
        // An unparseable purge_after counts as absent, an unparseable created_at never expires.
        assert!(snapshot_expired(Some("junk"), c, t(2027, 1, 1)));
        assert!(!snapshot_expired(None, "junk", t(2099, 1, 1)));
        assert!(!snapshot_expired(Some("junk"), "junk", t(2099, 1, 1)));
    }

    #[test]
    fn the_parked_set_is_the_three_waiting_statuses() {
        assert_eq!(
            PARKED_STATUSES,
            ["awaiting_user", "awaiting_schedule", "paused"]
        );
        assert_eq!(
            parked_status_list_sql(),
            "'awaiting_user','awaiting_schedule','paused'"
        );
    }

    #[tokio::test]
    async fn an_awaiting_user_run_expires_on_time_and_loses_snapshots_and_input() {
        with_root!({
            let (u, p) = ("ret-u1", "ret-p1");
            setup(u, p).await;
            let mut c = conn(u, p).await;
            add_run(&mut c, "r", "awaiting_user", "2026-11-01T00:00:00+00:00").await;
            let created = t(2026, 11, 1);
            let pa = purge_after_for(CheckpointPark::AwaitingUser, created).unwrap();
            add_snapshot(&mut c, "r", Some(&pa), &created.to_rfc3339()).await;
            let due = created + Duration::days(AWAITING_USER_RETENTION_DAYS);

            let before = expire_parked_run(&mut c, "r", due - Duration::seconds(1))
                .await
                .unwrap();
            assert_eq!(before, ExpireOutcome::NotExpired);
            assert_eq!(
                state(&mut c, "r").await,
                ("awaiting_user".into(), Some("window text".into()), 1)
            );

            let after = expire_parked_run(&mut c, "r", due + Duration::seconds(1))
                .await
                .unwrap();
            assert_eq!(after, ExpireOutcome::Expired);
            assert_eq!(state(&mut c, "r").await, ("cancelled".into(), None, 0));
            let row = sqlx::query("SELECT external_input, notes FROM focus_runs WHERE id='r'")
                .fetch_one(&mut c)
                .await
                .unwrap();
            assert_eq!(row.get::<Option<String>, _>("external_input"), None);
            assert!(row
                .get::<String, _>("notes")
                .contains(EXPIRED_WHILE_WAITING));
            // A second pass changes nothing (the run is no longer parked).
            assert_eq!(
                expire_parked_run(&mut c, "r", due + Duration::days(9))
                    .await
                    .unwrap(),
                ExpireOutcome::NotParked
            );
        });
    }

    #[tokio::test]
    async fn an_awaiting_schedule_run_is_not_purged_before_scheduled_for_plus_thirty_days() {
        with_root!({
            let (u, p) = ("ret-u2", "ret-p2");
            setup(u, p).await;
            let mut c = conn(u, p).await;
            add_run(
                &mut c,
                "r",
                "awaiting_schedule",
                "2026-01-01T00:00:00+00:00",
            )
            .await;
            // Parked 100 days ago for a time still 60 days ahead of that.
            let parked_at = t(2026, 1, 1);
            let scheduled = t(2026, 3, 1);
            let pa =
                purge_after_for(CheckpointPark::AwaitingSchedule(scheduled), parked_at).unwrap();
            add_snapshot(&mut c, "r", Some(&pa), &parked_at.to_rfc3339()).await;
            let due = scheduled + Duration::days(AWAITING_SCHEDULE_RETENTION_DAYS);

            // Past created_at + 30 days, before scheduled_for + 30 days: kept.
            let early = parked_at + Duration::days(45);
            assert_eq!(
                expire_parked_run(&mut c, "r", early).await.unwrap(),
                ExpireOutcome::NotExpired
            );
            assert_eq!(
                expire_parked_run(&mut c, "r", due - Duration::seconds(1))
                    .await
                    .unwrap(),
                ExpireOutcome::NotExpired
            );
            assert_eq!(
                expire_parked_run(&mut c, "r", due + Duration::seconds(1))
                    .await
                    .unwrap(),
                ExpireOutcome::Expired
            );
        });
    }

    /// The gap: a crashed run is demoted to paused and its latest snapshot is a
    /// periodic one with no purge_after.
    #[tokio::test]
    async fn a_crashed_run_demoted_to_paused_expires_by_its_created_at() {
        with_root!({
            let (u, p) = ("ret-u3", "ret-p3");
            setup(u, p).await;
            let mut c = conn(u, p).await;
            add_run(&mut c, "r", "running", "2026-11-01T00:00:00+00:00").await;
            let created = t(2026, 11, 1);
            add_snapshot(&mut c, "r", None, &created.to_rfc3339()).await;
            drop(c);

            // The real startup recovery turns it into paused.
            let n = crate::conductor::lifecycle::demote_interrupted_runs(u, p, KEY_HEX)
                .await
                .unwrap();
            assert_eq!(n, Some(1));
            let mut c = conn(u, p).await;
            assert_eq!(state(&mut c, "r").await.0, "paused");

            let due = created + Duration::days(AWAITING_USER_RETENTION_DAYS);
            assert_eq!(
                expire_parked_run(&mut c, "r", due - Duration::seconds(1))
                    .await
                    .unwrap(),
                ExpireOutcome::NotExpired
            );
            assert_eq!(
                expire_parked_run(&mut c, "r", due + Duration::seconds(1))
                    .await
                    .unwrap(),
                ExpireOutcome::Expired
            );
            assert_eq!(state(&mut c, "r").await, ("cancelled".into(), None, 0));
        });
    }

    /// A run that resumed (or finished) between the sweep's listing and its
    /// expiry is left alone: the guarded UPDATE changes nothing, so nothing is
    /// deleted. A run that re-parked with a fresh snapshot is not expired either.
    #[tokio::test]
    async fn a_run_that_resumed_meanwhile_is_left_alone() {
        with_root!({
            let (u, p) = ("ret-u4", "ret-p4");
            setup(u, p).await;
            let mut c = conn(u, p).await;
            let old = t(2026, 1, 1);
            let old_pa = purge_after_for(CheckpointPark::AwaitingUser, old).unwrap();
            let now = t(2026, 6, 1);
            for (id, status) in [
                ("resumed", "running"),
                ("done", "complete"),
                ("failed", "failed"),
            ] {
                add_run(&mut c, id, status, "2026-01-01T00:00:00+00:00").await;
                add_snapshot(&mut c, id, Some(&old_pa), &old.to_rfc3339()).await;
            }
            for id in ["resumed", "done", "failed"] {
                assert_eq!(
                    expire_parked_run(&mut c, id, now).await.unwrap(),
                    ExpireOutcome::NotParked,
                    "{id}"
                );
                let (status, input, n) = state(&mut c, id).await;
                assert_ne!(status, "cancelled");
                assert_eq!(input.as_deref(), Some("window text"));
                assert_eq!(n, 1, "{id} keeps its snapshot");
            }
            // Re-parked: the latest snapshot is fresh.
            add_run(
                &mut c,
                "reparked",
                "awaiting_user",
                "2026-01-01T00:00:00+00:00",
            )
            .await;
            add_snapshot(&mut c, "reparked", Some(&old_pa), &old.to_rfc3339()).await;
            let fresh =
                purge_after_for(CheckpointPark::AwaitingUser, now - Duration::days(1)).unwrap();
            add_snapshot(
                &mut c,
                "reparked",
                Some(&fresh),
                &(now - Duration::days(1)).to_rfc3339(),
            )
            .await;
            assert_eq!(
                expire_parked_run(&mut c, "reparked", now).await.unwrap(),
                ExpireOutcome::NotExpired
            );
            assert_eq!(state(&mut c, "reparked").await.2, 2);
            // A parked run with no snapshot at all is not touched.
            add_run(&mut c, "bare", "awaiting_user", "2026-05-20T00:00:00+00:00").await;
            assert_eq!(
                expire_parked_run(&mut c, "bare", now).await.unwrap(),
                ExpireOutcome::NotExpired
            );
        });
    }

    async fn registry_with_key(user_id: &str) -> KeyRegistry {
        let reg = KeyRegistry::default();
        reg.replace(UnlockedKey {
            user_id: user_id.to_owned(),
            data_key: data_key(),
            key_epoch: 1,
            sharing_private_key: [0u8; 32],
            unlocked_at: crate::providers::utils::now(),
        })
        .await;
        reg
    }

    #[tokio::test]
    async fn the_tick_expires_with_a_key_resident_and_does_nothing_without_one() {
        with_root!({
            let (u, p) = ("ret-u5", "ret-p5");
            let pool = setup(u, p).await;
            let mut c = conn(u, p).await;
            let old = t(2026, 1, 1);
            let pa = purge_after_for(CheckpointPark::AwaitingUser, old).unwrap();
            add_run(&mut c, "r", "awaiting_user", "2026-01-01T00:00:00+00:00").await;
            add_snapshot(&mut c, "r", Some(&pa), &old.to_rfc3339()).await;
            add_run(&mut c, "live", "running", "2026-01-01T00:00:00+00:00").await;
            add_snapshot(&mut c, "live", Some(&pa), &old.to_rfc3339()).await;
            let now = t(2026, 6, 1);

            // No key resident: nothing happens, and the callback never fires.
            let empty = KeyRegistry::default();
            let mut told = Vec::new();
            expire_tick(&pool, &empty, now, |pe, r| {
                told.push((pe.to_owned(), r.to_owned()))
            })
            .await;
            assert!(told.is_empty());
            assert_eq!(state(&mut c, "r").await.0, "awaiting_user");
            assert_eq!(state(&mut c, "r").await.2, 1);

            // Key resident: the parked run expires, the running one is untouched.
            let reg = registry_with_key(u).await;
            expire_tick(&pool, &reg, now, |pe, r| {
                told.push((pe.to_owned(), r.to_owned()))
            })
            .await;
            assert_eq!(told, vec![(p.to_owned(), "r".to_owned())]);
            assert_eq!(state(&mut c, "r").await, ("cancelled".into(), None, 0));
            assert_eq!(
                state(&mut c, "live").await,
                ("running".into(), Some("window text".into()), 1)
            );
        });
    }

    #[test]
    fn the_sweep_has_one_clock_one_set_and_a_guarded_update() {
        let store = include_str!("../persistence/output_store.rs");
        let cancel = store
            .split("async fn cancel_parked_in_tx(")
            .nth(1)
            .and_then(|r| r.split("pub async fn cancel_parked_run(").next())
            .unwrap();
        assert!(cancel.contains("WHERE id = ? AND status IN ({})"));
        assert!(cancel.contains("if changed != 1"));
        let expire = store
            .split("pub async fn expire_parked_run(")
            .nth(1)
            .and_then(|r| r.split("pub async fn expire_parked_runs(").next())
            .unwrap();
        assert!(expire.contains("begin_write"));
        assert!(expire.contains("cancel_parked_in_tx("));
        assert!(!expire.contains("Utc::now"), "the clock is injected");
        let me = include_str!("run_retention.rs");
        let live = me.split("#[cfg(test)]").next().unwrap();
        assert_eq!(
            live.matches("Utc::now()").count(),
            1,
            "only the supervised tick reads the clock"
        );
        assert!(live.contains("Kind::Sweep"));
    }

    #[test]
    fn a_run_with_no_snapshot_expires_at_started_at_plus_thirty_days() {
        let started = "2026-11-01T12:00:00.5+00:00";
        let due = t(2026, 12, 1);
        assert!(!run_expired_without_snapshot(started, due));
        assert!(run_expired_without_snapshot(
            started,
            due + Duration::seconds(1)
        ));
        // An unparseable started_at is never expired.
        assert!(!run_expired_without_snapshot("junk", t(2099, 1, 1)));
        assert!(!run_expired_without_snapshot("", t(2099, 1, 1)));
    }

    #[tokio::test]
    async fn a_parked_run_with_no_snapshot_expires_by_started_at() {
        with_root!({
            let (u, p) = ("ret-u6", "ret-p6");
            setup(u, p).await;
            let mut c = conn(u, p).await;
            let started = "2026-11-01T12:00:00+00:00";
            for (id, status) in [
                ("au", "awaiting_user"),
                ("as", "awaiting_schedule"),
                ("pa", "paused"),
                ("rn", "running"),
            ] {
                add_run(&mut c, id, status, started).await;
            }
            add_run(&mut c, "bad", "awaiting_user", "not a time").await;
            let due = t(2026, 12, 1);
            for id in ["au", "as", "pa", "rn", "bad"] {
                assert_eq!(
                    expire_parked_run(&mut c, id, due - Duration::seconds(1))
                        .await
                        .unwrap(),
                    ExpireOutcome::NotExpired,
                    "{id} before"
                );
            }
            let after = due + Duration::seconds(1);
            for id in ["au", "as", "pa"] {
                assert_eq!(
                    expire_parked_run(&mut c, id, after).await.unwrap(),
                    ExpireOutcome::Expired,
                    "{id}"
                );
                assert_eq!(state(&mut c, id).await, ("cancelled".into(), None, 0));
            }
            // A running run is past its time but not parked: left alone.
            assert_eq!(
                expire_parked_run(&mut c, "rn", after).await.unwrap(),
                ExpireOutcome::NotParked
            );
            assert_eq!(state(&mut c, "rn").await.1.as_deref(), Some("window text"));
            // An unparseable started_at is never expired, however late.
            assert_eq!(
                expire_parked_run(&mut c, "bad", t(2099, 1, 1))
                    .await
                    .unwrap(),
                ExpireOutcome::NotExpired
            );
        });
    }

    #[test]
    fn formatted_values_compare_as_times() {
        let a = format_purge_after(t(2026, 11, 19));
        let b = format_purge_after(t(2026, 12, 1));
        assert!(a < b);
        assert_eq!(a.len(), b.len());
    }
}
