// src-tauri/src/conductor/run_retention.rs
//
// items.id=681 (decisions.id=865): retention of a PARKED run's snapshots.
//
// A snapshot (focus_run_snapshots) holds step outputs and disclosure buffers
// with the values approved for the step, encrypted at rest in the persona's
// outputs.db. A run keeps snapshots while it runs (the periodic checkpoints,
// until output() purges them) and while it is parked (awaiting_user,
// awaiting_schedule, paused). complete, cancelled and failed delete them when the
// status is written: cleanup() and fail_run() in lifecycle.rs, the cancel writers
// in output_store.rs. Until items.id=888 stage 1 this was FALSE for a run that
// failed through execute_full's error arms (F5 of the 2026-11-20 privacy review):
// the snapshot outlived it. The delete is best effort: a purge that fails leaves
// the snapshot, and no sweep selects a finished run. output() purges before
// awaiting_feedback, so a post-output run (awaiting_feedback,
// awaiting_extract_confirm) has none; items.id=888 settles those runs
// (POST_OUTPUT_STATUSES, output_store::sweep_post_output_runs). This module owns
// WHEN a parked run's snapshot expires:
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
use crate::persistence::output_store::{expire_parked_runs, sweep_post_output_runs};
use crate::persistence::persona_store::list_persona_ids_for_user;

/// The plain message a run carries when its snapshot expired while it waited.
pub const EXPIRED_WHILE_WAITING: &str = "expired while waiting";

/// Statuses a sweep may expire: runs waiting for the user, for a schedule, or
/// paused by a crash recovery.
pub const PARKED_STATUSES: &[&str] = &["awaiting_user", "awaiting_schedule", "paused"];

/// items.id=888 stage 7: days a row of session_variables (the one-time latch exempt),
/// pass_artifacts or plan_approvals is kept after it was last written (updated_at), the
/// ONE period of the store sweep (persistence::store_sweep).
pub const STORE_RETENTION_DAYS: i64 = 30;

/// items.id=888 (F8): runs whose reply was saved (output() ran) and that wait for
/// nothing that can arrive: awaiting_extract_confirm (no confirm screen yet,
/// items.id=38) and awaiting_feedback (normally overwritten by `complete` at once, left
/// behind only by an error or crash in the extraction pass). The sweep SETTLES them as
/// `complete` at started_at + POST_OUTPUT_RETENTION_DAYS (output_store::
/// sweep_post_output_runs). Disjoint from RESUMABLE_STATUSES and PARKED_STATUSES (a
/// test pins it), so a run is never both settled and resumed or cancelled as parked.
pub const POST_OUTPUT_STATUSES: &[&str] = &["awaiting_extract_confirm", "awaiting_feedback"];

/// items.id=888 (F8): days after started_at at which a post-output run is settled.
pub const POST_OUTPUT_RETENTION_DAYS: i64 = 30;

/// items.id=888 (F8): whether a post-output run started at `started_at` is past its
/// retention at `now`. An unparseable started_at is never settled (its input is still
/// cleared by the pass for awaiting_extract_confirm).
pub fn post_output_run_expired(started_at: &str, now: DateTime<Utc>) -> bool {
    match DateTime::parse_from_rfc3339(started_at) {
        Ok(s) => now > s.with_timezone(&Utc) + Duration::days(POST_OUTPUT_RETENTION_DAYS),
        Err(_) => false,
    }
}

/// What a sweep did to a run it reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Swept {
    /// A parked run expired and was cancelled (items.id=681).
    Cancelled,
    /// A post-output run was settled as complete (items.id=888).
    Settled,
}

/// items.id=888 (F7): the statuses a rehydrated run may be RESUMED from (the guarded
/// claim, output_store::claim_run). `running` and `initializing` are not here: a run
/// in either is live, or was left by a crash and becomes `paused` at the next login
/// (demote_interrupted_runs). Kept apart from PARKED_STATUSES, which is what the sweep
/// expires, because the two sets need not stay equal.
pub const RESUMABLE_STATUSES: &[&str] = &["awaiting_user", "awaiting_schedule", "paused"];

/// items.id=888 (F7): the statuses reenter_step may rewind from: the resumable ones and
/// awaiting_feedback (a revision after the output, items.id=574). Never a live status.
pub const REENTERABLE_STATUSES: &[&str] = &[
    "awaiting_user",
    "awaiting_schedule",
    "awaiting_feedback",
    "paused",
];

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
    mut on_expired: impl FnMut(&str, &str, Swept),
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
                    on_expired(&persona_id, id, Swept::Cancelled);
                }
            }
            Err(e) => log::debug!("run_retention: persona '{persona_id}' not swept: {e}"),
        }
        // items.id=888 stage 7: the chat stores, in the same tick and lease. A sweep that
        // fails deletes nothing (one transaction) and is tried again next tick.
        if let Err(e) =
            crate::persistence::store_sweep::sweep_stores(&user_id, &persona_id, &key_hex_str, now)
                .await
        {
            log::debug!("run_retention: persona '{persona_id}' store sweep skipped: {e}");
        }
        // items.id=888 (F8): the post-output runs, in the same tick and lease.
        match sweep_post_output_runs(&user_id, &persona_id, &key_hex_str, now).await {
            Ok(report) => {
                for id in &report.settled {
                    on_expired(&persona_id, id, Swept::Settled);
                }
            }
            Err(e) => {
                log::debug!("run_retention: persona '{persona_id}' post-output sweep skipped: {e}")
            }
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
    expire_tick(pool, key_registry, Utc::now(), |_persona, run_id, swept| {
        if let Some(h) = &app_handle {
            match swept {
                Swept::Cancelled => emit_expired(h, run_id),
                Swept::Settled => emit_swept(h, run_id, "complete", EXPIRED_WHILE_WAITING),
            }
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
    emit_swept(handle, run_id, "cancelled", message);
}

/// Emit "run-status-update" for a run a sweep moved to `status` (cancelled, or
/// complete for a settled post-output run), with the plain message.
fn emit_swept(handle: &tauri::AppHandle<tauri::Wry>, run_id: &str, status: &str, message: &str) {
    use tauri::Emitter;
    let payload = crate::conductor::lifecycle::RunStatusPayload {
        focus_run_id: run_id.to_owned(),
        status: status.to_owned(),
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
            expire_tick(&pool, &empty, now, |pe, r, _| {
                told.push((pe.to_owned(), r.to_owned()))
            })
            .await;
            assert!(told.is_empty());
            assert_eq!(state(&mut c, "r").await.0, "awaiting_user");
            assert_eq!(state(&mut c, "r").await.2, 1);

            // Key resident: the parked run expires, the running one is untouched.
            let reg = registry_with_key(u).await;
            expire_tick(&pool, &reg, now, |pe, r, swept| {
                assert_eq!(swept, Swept::Cancelled);
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

    // ----- items.id=888 stage 3 (F8): runs that ended their output and wait -------

    use crate::persistence::output_store::sweep_post_output_runs;

    async fn add_candidate(c: &mut sqlx::SqliteConnection, run_id: &str, status: &str) {
        sqlx::query(
            "INSERT INTO extract_confirm_candidates
             (focus_run_id, field_name, extracted_value, sensitivity, reason, confidence,
              source, status, created_at, updated_at)
             VALUES (?, 'employer', 'Acme', 'personal', 'r', 0.9, 'conductor_batch', ?, 't', 't')",
        )
        .bind(run_id)
        .bind(status)
        .execute(c)
        .await
        .unwrap();
    }

    async fn candidates(c: &mut sqlx::SqliteConnection, run_id: &str) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM extract_confirm_candidates WHERE focus_run_id = ?")
            .bind(run_id)
            .fetch_one(c)
            .await
            .unwrap()
    }

    async fn inputs(c: &mut sqlx::SqliteConnection, id: &str) -> (Option<String>, Option<String>) {
        sqlx::query_as("SELECT user_input, external_input FROM focus_runs WHERE id = ?")
            .bind(id)
            .fetch_one(c)
            .await
            .unwrap()
    }

    async fn reason(c: &mut sqlx::SqliteConnection, id: &str) -> Option<String> {
        sqlx::query_scalar(
            "SELECT json_extract(notes, '$.cancel_reason') FROM focus_runs WHERE id = ?",
        )
        .bind(id)
        .fetch_one(c)
        .await
        .unwrap()
    }

    #[test]
    fn the_post_output_set_and_period() {
        assert_eq!(
            POST_OUTPUT_STATUSES,
            &["awaiting_extract_confirm", "awaiting_feedback"]
        );
        assert_eq!(POST_OUTPUT_RETENTION_DAYS, 30);
        let started = "2026-01-01T00:00:00+00:00";
        assert!(!post_output_run_expired(started, t(2026, 1, 30)));
        assert!(post_output_run_expired(started, t(2026, 2, 1)));
        assert!(!post_output_run_expired("not a time", t(2030, 1, 1)));
    }

    /// The settle and the resume claim never act on the same status, so a run cannot be
    /// both settled and resumed (and the sweep's cancel of a parked run cannot settle
    /// one either).
    #[test]
    fn the_settled_statuses_are_disjoint_from_the_resumable_and_parked_ones() {
        for s in POST_OUTPUT_STATUSES {
            assert!(!RESUMABLE_STATUSES.contains(s), "{s}");
            assert!(!PARKED_STATUSES.contains(s), "{s}");
        }
    }

    /// Every pass clears the inputs of a run at awaiting_extract_confirm, whatever its
    /// age, and changes nothing else about it. An awaiting_feedback run keeps its input
    /// until it is settled, and no other status is touched.
    #[tokio::test]
    async fn every_pass_clears_the_inputs_of_an_extract_confirm_run_at_once() {
        with_root!({
            let (u, p) = ("ret-u8", "ret-p8");
            let _pool = setup(u, p).await;
            let mut c = conn(u, p).await;
            let now = t(2026, 6, 1);
            let fresh = now.to_rfc3339();
            add_run(&mut c, "confirm", "awaiting_extract_confirm", &fresh).await;
            add_candidate(&mut c, "confirm", "pending").await;
            add_run(&mut c, "feedback", "awaiting_feedback", &fresh).await;
            add_run(&mut c, "parked", "awaiting_user", &fresh).await;
            let report = sweep_post_output_runs(u, p, KEY_HEX, now).await.unwrap();
            assert_eq!(report.inputs_cleared, 1);
            assert!(report.settled.is_empty());
            assert_eq!(inputs(&mut c, "confirm").await, (None, None));
            assert_eq!(state(&mut c, "confirm").await.0, "awaiting_extract_confirm");
            assert_eq!(
                candidates(&mut c, "confirm").await,
                1,
                "kept until it is settled"
            );
            for id in ["feedback", "parked"] {
                assert_eq!(
                    inputs(&mut c, id).await,
                    (Some("window text".into()), Some("user turns".into())),
                    "{id}"
                );
            }
            // A second pass has nothing to clear.
            let again = sweep_post_output_runs(u, p, KEY_HEX, now).await.unwrap();
            assert_eq!(again.inputs_cleared, 0);
        });
    }

    /// At started_at + 30 days a run at awaiting_extract_confirm or awaiting_feedback is
    /// settled as complete with the reason in notes, keeps no input, no snapshot and no
    /// candidate row of ANY status (pending, declined, confirmed). A younger run and
    /// another run's candidates are left alone.
    #[tokio::test]
    async fn post_output_runs_settle_as_complete_after_thirty_days_and_lose_every_candidate() {
        with_root!({
            let (u, p) = ("ret-u9", "ret-p9");
            let _pool = setup(u, p).await;
            let mut c = conn(u, p).await;
            let now = t(2026, 6, 1);
            let old = "2026-04-01T00:00:00+00:00";
            let young = "2026-05-10T00:00:00+00:00";
            add_run(&mut c, "old-confirm", "awaiting_extract_confirm", old).await;
            for status in ["pending", "declined", "confirmed"] {
                add_candidate(&mut c, "old-confirm", status).await;
            }
            add_snapshot(&mut c, "old-confirm", None, old).await;
            add_run(&mut c, "old-feedback", "awaiting_feedback", old).await;
            add_run(&mut c, "young-confirm", "awaiting_extract_confirm", young).await;
            add_candidate(&mut c, "young-confirm", "pending").await;
            add_run(&mut c, "young-feedback", "awaiting_feedback", young).await;

            let report = sweep_post_output_runs(u, p, KEY_HEX, now).await.unwrap();
            let mut settled = report.settled.clone();
            settled.sort();
            assert_eq!(settled, ["old-confirm", "old-feedback"]);
            for id in ["old-confirm", "old-feedback"] {
                assert_eq!(
                    state(&mut c, id).await,
                    ("complete".into(), None, 0),
                    "{id}"
                );
                assert_eq!(inputs(&mut c, id).await, (None, None), "{id}");
                assert_eq!(
                    reason(&mut c, id).await.as_deref(),
                    Some(EXPIRED_WHILE_WAITING)
                );
            }
            assert_eq!(candidates(&mut c, "old-confirm").await, 0);
            assert_eq!(candidates(&mut c, "young-confirm").await, 1);
            assert_eq!(
                state(&mut c, "young-confirm").await.0,
                "awaiting_extract_confirm"
            );
            assert_eq!(state(&mut c, "young-feedback").await.0, "awaiting_feedback");
            assert_eq!(
                inputs(&mut c, "young-feedback").await,
                (Some("window text".into()), Some("user turns".into()))
            );
        });
    }

    /// A run cannot be both settled and resumed or re-entered: the claim refuses a
    /// settled run, and a run that a claim or a re-entry moved on (running,
    /// awaiting_user) is never settled, however old it is.
    #[tokio::test]
    async fn a_settled_run_cannot_be_claimed_and_a_claimed_run_is_not_settled() {
        with_root!({
            let (u, p) = ("ret-u10", "ret-p10");
            let _pool = setup(u, p).await;
            let mut c = conn(u, p).await;
            let now = t(2026, 6, 1);
            let old = "2026-01-01T00:00:00+00:00";
            add_run(&mut c, "fb", "awaiting_feedback", old).await;
            add_run(&mut c, "fb-reentered", "awaiting_feedback", old).await;
            // A re-entry claims fb-reentered first (awaiting_feedback -> awaiting_user).
            assert!(crate::persistence::output_store::claim_run(
                &mut c,
                "fb-reentered",
                "awaiting_user",
                "awaiting_feedback",
                REENTERABLE_STATUSES,
                None,
            )
            .await
            .unwrap());
            add_run(&mut c, "claimed", "running", old).await;

            let report = sweep_post_output_runs(u, p, KEY_HEX, now).await.unwrap();
            assert_eq!(report.settled, ["fb"]);
            assert_eq!(state(&mut c, "fb-reentered").await.0, "awaiting_user");
            assert_eq!(state(&mut c, "claimed").await.0, "running");
            // The settled run cannot be claimed for a resume or a re-entry afterwards.
            for (to, allowed) in [
                ("running", RESUMABLE_STATUSES),
                ("awaiting_user", REENTERABLE_STATUSES),
            ] {
                assert!(!crate::persistence::output_store::claim_run(
                    &mut c,
                    "fb",
                    to,
                    "awaiting_feedback",
                    allowed,
                    None,
                )
                .await
                .unwrap());
            }
            assert_eq!(state(&mut c, "fb").await.0, "complete");
        });
    }

    /// The hourly tick settles post-output runs too and reports them as Settled, with
    /// the key resident only.
    #[tokio::test]
    async fn the_tick_settles_post_output_runs_with_a_key_resident_only() {
        with_root!({
            let (u, p) = ("ret-u11", "ret-p11");
            let pool = setup(u, p).await;
            let mut c = conn(u, p).await;
            add_run(
                &mut c,
                "conf",
                "awaiting_extract_confirm",
                "2026-01-01T00:00:00+00:00",
            )
            .await;
            add_candidate(&mut c, "conf", "pending").await;
            let now = t(2026, 6, 1);
            let mut told = Vec::new();
            expire_tick(&pool, &KeyRegistry::default(), now, |_, r, s| {
                told.push((r.to_owned(), s))
            })
            .await;
            assert!(told.is_empty());
            assert_eq!(
                state(&mut c, "conf").await.1.as_deref(),
                Some("window text")
            );

            let reg = registry_with_key(u).await;
            expire_tick(&pool, &reg, now, |_, r, s| told.push((r.to_owned(), s))).await;
            assert_eq!(told, vec![("conf".to_owned(), Swept::Settled)]);
            assert_eq!(state(&mut c, "conf").await, ("complete".into(), None, 0));
            assert_eq!(candidates(&mut c, "conf").await, 0);
        });
    }

    /// items.id=888 stage 7: the tick sweeps the chat stores only with the owner's key
    /// resident; the latch is exempt.
    #[tokio::test]
    async fn the_tick_sweeps_the_chat_stores_only_with_the_key_resident() {
        with_root!({
            let (u, p) = ("ret-u12", "ret-p12");
            let pool = setup(u, p).await;
            let mut c = conn(u, p).await;
            let now = t(2026, 6, 1);
            let old = (now - Duration::days(STORE_RETENTION_DAYS + 1)).to_rfc3339();
            sqlx::query(
                "INSERT INTO session_variables (context_key, focus_id, name, value, version,
                                                written_by_run, created_at, updated_at)
                 VALUES ('chat-s', 'f', 'locked_intent', 'billing', 1, 'r', ?, ?)",
            )
            .bind(&old)
            .bind(&old)
            .execute(&mut c)
            .await
            .unwrap();
            crate::persistence::session_store::latch_write_suppressed(&mut c, "chat-s", "f", "r")
                .await
                .unwrap();
            sqlx::query("UPDATE session_variables SET updated_at = ? WHERE name = 'write_mode'")
                .bind(&old)
                .execute(&mut c)
                .await
                .unwrap();
            let q = "SELECT name FROM session_variables ORDER BY name";
            expire_tick(&pool, &KeyRegistry::default(), now, |_, _, _| {}).await;
            let names: Vec<String> = sqlx::query_scalar(q).fetch_all(&mut c).await.unwrap();
            assert_eq!(
                names,
                ["locked_intent", "write_mode"],
                "no key: nothing swept"
            );
            let reg = registry_with_key(u).await;
            expire_tick(&pool, &reg, now, |_, _, _| {}).await;
            let names: Vec<String> = sqlx::query_scalar(q).fetch_all(&mut c).await.unwrap();
            assert_eq!(names, ["write_mode"], "key resident: swept, latch exempt");
        });
    }

    /// The login pass runs the post-output sweep right after the parked-run expiry.
    #[test]
    fn the_login_pass_sweeps_post_output_runs_too() {
        let auth = include_str!("../commands/auth.rs");
        let expire = auth
            .find("crate::persistence::output_store::expire_parked_runs(")
            .expect("expiry");
        let settle = auth
            .find("crate::persistence::output_store::sweep_post_output_runs(")
            .expect("post-output sweep");
        assert!(expire < settle);
    }

    /// The settle is one guarded UPDATE on the post-output set, and only when it changed
    /// exactly one row are the candidates, snapshots, park and pending review removed.
    #[test]
    fn the_settle_is_a_guarded_update() {
        let store = include_str!("../persistence/output_store.rs");
        let settle = store
            .split("async fn settle_post_output_in_tx(")
            .nth(1)
            .and_then(|r| r.split("\n}\n").next())
            .unwrap();
        assert!(
            settle.contains("SET status = 'complete', user_input = NULL, external_input = NULL")
        );
        assert!(settle.contains("WHERE id = ? AND status IN ({})"));
        let guard = settle.find("if changed != 1").expect("guard");
        let del = settle
            .find("DELETE FROM extract_confirm_candidates WHERE focus_run_id = ?")
            .expect("candidate delete");
        assert!(guard < del);
        assert!(
            !settle.contains("AND status = 'pending'"),
            "every candidate, any status"
        );
    }
}
