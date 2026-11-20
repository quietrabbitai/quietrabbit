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
// Stage 1 (this file today) only WRITES purge_after. The sweeper that deletes
// expired snapshots is not built yet.

use chrono::{DateTime, Duration, SecondsFormat, Utc};

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

    #[test]
    fn formatted_values_compare_as_times() {
        let a = format_purge_after(t(2026, 11, 19));
        let b = format_purge_after(t(2026, 12, 1));
        assert!(a < b);
        assert_eq!(a.len(), b.len());
    }
}
