// src-tauri/src/conductor/nightly_batch.rs
//
// items.id=52 Part 1 -- nightly batch runner infrastructure. Fires once
// per calendar day at instance_config.nightly_batch_hour (local time,
// default 2 = 2am), wall-clock gated the same way auth::idle_timeout does:
// a persisted last-run timestamp is compared against a freshly computed
// boundary on every tick, never a tick counter -- so a sleep/suspend, or a
// missed tick because QR simply wasn't running at the scheduled hour, is
// transparent. The first tick after the gap sees the full elapsed time and
// fires once for that day rather than skipping it silently.
//
// Part 1 scope only: this self-check confirms qr-admin.focus resolves via
// load_focus_definition -- the identity anchor items.id=52's scoping doc
// calls for -- and updates its own schedule state. It creates no Topic or
// output row; items.id=52 Part 3+ (quality re-assessment, gap-detection,
// dedup) is what will have real findings to write, once built.

use chrono::{DateTime, Local, LocalResult, TimeZone, Utc};

const DEFAULT_HOUR: u32 = 2;

async fn read_nightly_batch_hour(pool: &sqlx::SqlitePool) -> u32 {
    let row: Option<(String,)> =
        sqlx::query_as("SELECT value FROM instance_config WHERE key = 'nightly_batch_hour'")
            .fetch_optional(pool)
            .await
            .unwrap_or(None);

    match row.and_then(|(v,)| v.parse::<u32>().ok()) {
        Some(hour) if hour <= 23 => hour,
        Some(bad) => {
            log::warn!(
                "nightly_batch: instance_config.nightly_batch_hour={bad} out of range, \
                 using default {DEFAULT_HOUR}"
            );
            DEFAULT_HOUR
        }
        None => DEFAULT_HOUR,
    }
}

async fn read_last_run_at(pool: &sqlx::SqlitePool) -> Option<DateTime<Utc>> {
    let row: Option<(String,)> =
        sqlx::query_as("SELECT value FROM instance_config WHERE key = 'nightly_batch_last_run_at'")
            .fetch_optional(pool)
            .await
            .unwrap_or(None);

    row.and_then(|(v,)| DateTime::parse_from_rfc3339(&v).ok())
        .map(|dt| dt.with_timezone(&Utc))
}

async fn write_last_run_at(pool: &sqlx::SqlitePool, now: DateTime<Utc>) {
    if let Err(e) =
        sqlx::query("UPDATE instance_config SET value = ? WHERE key = 'nightly_batch_last_run_at'")
            .bind(now.to_rfc3339())
            .execute(pool)
            .await
    {
        log::error!("nightly_batch: failed to persist last_run_at: {e}");
    }
}

/// Pure boundary check, no DB: is today's `{hour}:00:00` local window due,
/// given the last completed run (if any)? Kept separate from its DB-reading
/// caller for the same reason auth::idle_timeout::is_idle_expired is --
/// directly unit-testable without a database.
fn is_due(hour: u32, last_run_at: Option<DateTime<Utc>>, now_local: DateTime<Local>) -> bool {
    let Some(naive_boundary) = now_local.date_naive().and_hms_opt(hour, 0, 0) else {
        log::error!("nightly_batch: could not construct {hour}:00:00, skipping tick");
        return false;
    };

    // DST edge cases: an ambiguous fall-back hour picks the earlier valid
    // interpretation; a spring-forward gap that makes the hour not exist
    // today skips this tick and self-corrects on the next one.
    let boundary_local = match Local.from_local_datetime(&naive_boundary) {
        LocalResult::Single(dt) => dt,
        LocalResult::Ambiguous(earliest, _latest) => earliest,
        LocalResult::None => {
            log::warn!(
                "nightly_batch: {hour}:00:00 local does not exist today (DST gap), \
                 skipping tick"
            );
            return false;
        }
    };

    if now_local < boundary_local {
        return false; // not due yet today
    }

    let boundary_utc = boundary_local.with_timezone(&Utc);
    !matches!(last_run_at, Some(last_run) if last_run >= boundary_utc) // already ran today's window
}

/// One sweep tick. Called on an interval by main.rs's spawn_supervised
/// loop -- the loop itself lives there, matching every other periodic
/// checker's split (idle_timeout_check, scheduled_step_sweep).
pub async fn run_periodic_sweep(pool: &sqlx::SqlitePool) {
    let hour = read_nightly_batch_hour(pool).await;
    let last_run_at = read_last_run_at(pool).await;

    if !is_due(hour, last_run_at, Local::now()) {
        return;
    }

    log::info!("nightly_batch: firing (scheduled_hour={hour} local)");
    match crate::conductor::lifecycle::load_focus_definition(pool, "qr-admin").await {
        Ok(_) => log::info!("nightly_batch: qr-admin focus identity resolved OK"),
        Err(e) => log::error!("nightly_batch: qr-admin.focus failed to load/validate: {e}"),
    }

    write_last_run_at(pool, Utc::now()).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::ENV_MUTEX;

    // ------------------------------------------------------------------
    // Pure boundary tests -- no DB.
    // ------------------------------------------------------------------

    fn local_dt(y: i32, mo: u32, d: u32, h: u32, mi: u32, s: u32) -> DateTime<Local> {
        Local
            .with_ymd_and_hms(y, mo, d, h, mi, s)
            .single()
            .expect("test fixture datetime must be unambiguous")
    }

    #[test]
    fn not_yet_due_before_scheduled_hour() {
        let now = local_dt(2026, 9, 27, 1, 59, 59);
        assert!(!is_due(2, None, now));
    }

    #[test]
    fn due_exactly_at_scheduled_hour_when_never_run() {
        let now = local_dt(2026, 9, 27, 2, 0, 0);
        assert!(is_due(2, None, now));
    }

    #[test]
    fn due_well_after_scheduled_hour_when_never_run() {
        let now = local_dt(2026, 9, 27, 9, 30, 0);
        assert!(is_due(2, None, now));
    }

    #[test]
    fn not_due_again_same_day_after_running() {
        let now = local_dt(2026, 9, 27, 9, 30, 0);
        let ran_today = local_dt(2026, 9, 27, 2, 0, 0).with_timezone(&Utc);
        assert!(!is_due(2, Some(ran_today), now));
    }

    #[test]
    fn due_again_next_day_after_running_yesterday() {
        let now = local_dt(2026, 9, 28, 2, 0, 1);
        let ran_yesterday = local_dt(2026, 9, 27, 2, 5, 0).with_timezone(&Utc);
        assert!(is_due(2, Some(ran_yesterday), now));
    }

    // ------------------------------------------------------------------
    // Integration test -- real shared.db, real qr-admin.focus file.
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn run_periodic_sweep_fires_and_resolves_qr_admin_focus() {
        let _lock = ENV_MUTEX.lock().await;
        let saved_root = std::env::var("QR_DATA_ROOT").ok();
        let tempdir = tempfile::tempdir().expect("failed to create tempdir");
        std::env::set_var("QR_DATA_ROOT", tempdir.path());

        crate::persistence::migrations::migrate_shared_db()
            .await
            .expect("shared.db migration must succeed in test setup");

        let pool =
            sqlx::SqlitePool::connect_with(crate::providers::utils::connect_options_unencrypted(
                &crate::providers::utils::db_path_shared(),
            ))
            .await
            .expect("shared.db pool must connect");

        // Hour 0 is always already past "today" no matter when this test
        // runs, and last_run_at is freshly seeded empty by the migration --
        // guaranteed due without needing to fake wall-clock time.
        sqlx::query("UPDATE instance_config SET value = '0' WHERE key = 'nightly_batch_hour'")
            .execute(&pool)
            .await
            .unwrap();

        run_periodic_sweep(&pool).await;

        let last_run: (String,) = sqlx::query_as(
            "SELECT value FROM instance_config WHERE key = 'nightly_batch_last_run_at'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(
            !last_run.0.is_empty(),
            "run_periodic_sweep must persist a last_run_at after firing"
        );

        match &saved_root {
            Some(v) => std::env::set_var("QR_DATA_ROOT", v),
            None => std::env::remove_var("QR_DATA_ROOT"),
        }
    }
}
