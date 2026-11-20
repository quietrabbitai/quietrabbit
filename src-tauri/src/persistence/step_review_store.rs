// src-tauri/src/persistence/step_review_store.rs
//
// items.id=884 (decisions.id=943 point 5): the typed store for step_reviews
// (schema/outputs_016.sql) in the persona's outputs.db, the record of the
// outbound review of the rendered prompt of an external Focus step.
//
// CONNECTIONS: every function takes a caller-supplied `&mut SqliteConnection` to
// the persona's outputs.db (output_store::open_outputs_db), so the persona is the
// file and a function can never read another persona's rows.
//
// WHAT THIS MODULE NEVER HOLDS: prompt text. A row has hashes, sizes, offsets,
// categories, counts and the replacement text of a span a standing decision
// settled (what leaves the device in place of the original, never the original).
// No error here carries a value, and the module logs nothing (a source scan pins
// it): conductor::step_review is the one place that logs a review, with counts
// and hash prefixes.
//
// STATE MACHINE (one writer, conductor::step_review):
//   insert(pending)  -> finalize(final outcome) | supersede | refuse
//   insert(final)    (a clean or standing-only review is written already final)
//   insert(blocked)  (the step parked, with a reason code)
// finalize, refuse and supersede act only on a row that is still pending, so a
// pending review is answered once. A final row is never edited.
//
// QUERY STYLE: runtime sqlx::query() only, never the query!() macros.

use serde::{Deserialize, Serialize};
use sqlx::{Row, SqliteConnection};
use thiserror::Error;
use uuid::Uuid;

use crate::providers::utils::now;

// ---------------------------------------------------------------------------
// Outcome vocabulary
// ---------------------------------------------------------------------------

/// The closed set of review outcomes (outputs_016.sql: held here, not as a SQL
/// CHECK).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ReviewOutcome {
    /// The filter found nothing. Nothing was changed.
    Clean,
    /// Every flagged span was settled by a standing decision. Nobody was asked.
    StandingApplied,
    /// The user answered every span and changed nothing the suggestion did not
    /// already say (released it, or took the suggested generalisation).
    ApprovedAsShown,
    /// The user wrote at least one replacement of their own. Wins over Redacted.
    Edited,
    /// At least one span was removed (kept private, or not answered) and none was
    /// edited.
    Redacted,
    /// The user refused. The run was cancelled.
    Refused,
    /// The step parked: the filter was unavailable, failed or timed out, or the
    /// answers could not be applied. Nothing was sent.
    Blocked,
    /// Internal: waiting for the user.
    Pending,
    /// Internal: replaced by a newer review of a changed text, or by a cancel.
    Superseded,
}

impl ReviewOutcome {
    pub const ALL: [ReviewOutcome; 9] = [
        ReviewOutcome::Clean,
        ReviewOutcome::StandingApplied,
        ReviewOutcome::ApprovedAsShown,
        ReviewOutcome::Edited,
        ReviewOutcome::Redacted,
        ReviewOutcome::Refused,
        ReviewOutcome::Blocked,
        ReviewOutcome::Pending,
        ReviewOutcome::Superseded,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            ReviewOutcome::Clean => "clean",
            ReviewOutcome::StandingApplied => "standing_applied",
            ReviewOutcome::ApprovedAsShown => "approved_as_shown",
            ReviewOutcome::Edited => "edited",
            ReviewOutcome::Redacted => "redacted",
            ReviewOutcome::Refused => "refused",
            ReviewOutcome::Blocked => "blocked",
            ReviewOutcome::Pending => "pending",
            ReviewOutcome::Superseded => "superseded",
        }
    }

    pub fn parse(s: &str) -> Option<ReviewOutcome> {
        ReviewOutcome::ALL.iter().copied().find(|o| o.as_str() == s)
    }

    /// An outcome that released text: the five that carry an outbound hash.
    pub fn is_final(self) -> bool {
        matches!(
            self,
            ReviewOutcome::Clean
                | ReviewOutcome::StandingApplied
                | ReviewOutcome::ApprovedAsShown
                | ReviewOutcome::Edited
                | ReviewOutcome::Redacted
        )
    }
}

// ---------------------------------------------------------------------------
// Row types
// ---------------------------------------------------------------------------

/// A span the user was asked about. No text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AskedSpan {
    pub span_id: String,
    pub start_byte: usize,
    pub end_byte: usize,
    pub category: String,
    /// "low" | "medium" | "high" (informational).
    pub review_tier: String,
}

/// A span a standing decision settled. `suggestion_text` and `user_modified_text`
/// are the REPLACEMENT that decision carries, not the original words.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SettledSpan {
    pub start_byte: usize,
    pub end_byte: usize,
    /// "generalize" | "keep_private" | "release_original"
    pub decision: String,
    pub suggestion_text: Option<String>,
    pub user_modified_text: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredSpans {
    pub asked: Vec<AskedSpan>,
    pub settled: Vec<SettledSpan>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counts {
    pub total: u32,
    pub auto_resolved: u32,
    pub released: u32,
    pub generalized: u32,
    pub removed: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepReviewRow {
    pub review_id: String,
    pub focus_run_id: String,
    pub action_id: String,
    pub prompt_sha: String,
    pub prompt_len: usize,
    pub spans: StoredSpans,
    pub counts: Counts,
    pub outcome: ReviewOutcome,
    pub reason: Option<String>,
    pub outbound_sha: Option<String>,
    pub outbound_len: Option<usize>,
    pub provider_id: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

/// What a new row is written with.
#[derive(Debug, Clone)]
pub struct NewReview<'a> {
    pub focus_run_id: &'a str,
    pub action_id: &'a str,
    pub prompt_sha: &'a str,
    pub prompt_len: usize,
    pub spans: &'a StoredSpans,
    pub counts: Counts,
    pub outcome: ReviewOutcome,
    pub reason: Option<&'a str>,
    pub outbound_sha: Option<&'a str>,
    pub outbound_len: Option<usize>,
    pub provider_id: Option<&'a str>,
}

/// What a pending row becomes when the answers are applied.
#[derive(Debug, Clone)]
pub struct Finalize<'a> {
    pub outcome: ReviewOutcome,
    pub counts: Counts,
    pub outbound_sha: &'a str,
    pub outbound_len: usize,
    pub provider_id: Option<&'a str>,
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// No variant carries a stored value.
#[derive(Debug, Error)]
pub enum StepReviewStoreError {
    #[error("Database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("step review row is not valid: {0}")]
    Invalid(&'static str),
}

// ---------------------------------------------------------------------------
// Writes
// ---------------------------------------------------------------------------

/// Insert a review. Returns its id. A `pending` or a final outcome must come
/// with the right fields: a final one carries an outbound hash and size, no other
/// outcome does.
pub async fn insert_review(
    conn: &mut SqliteConnection,
    new: &NewReview<'_>,
) -> Result<String, StepReviewStoreError> {
    if new.outcome.is_final() != new.outbound_sha.is_some() {
        return Err(StepReviewStoreError::Invalid(
            "only a final outcome carries an outbound hash",
        ));
    }
    if new.outbound_sha.is_some() != new.outbound_len.is_some() {
        return Err(StepReviewStoreError::Invalid(
            "an outbound hash and size go together",
        ));
    }
    let id = Uuid::new_v4().to_string();
    let ts = now();
    let spans_json = serde_json::to_string(new.spans)
        .map_err(|_| StepReviewStoreError::Invalid("spans do not serialise"))?;
    sqlx::query(
        "INSERT INTO step_reviews
         (review_id, focus_run_id, action_id, prompt_sha, prompt_len, spans_json,
          total_spans, auto_resolved, released, generalized, removed, outcome, reason,
          outbound_sha, outbound_len, provider_id, created_at, updated_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(new.focus_run_id)
    .bind(new.action_id)
    .bind(new.prompt_sha)
    .bind(new.prompt_len as i64)
    .bind(&spans_json)
    .bind(i64::from(new.counts.total))
    .bind(i64::from(new.counts.auto_resolved))
    .bind(i64::from(new.counts.released))
    .bind(i64::from(new.counts.generalized))
    .bind(i64::from(new.counts.removed))
    .bind(new.outcome.as_str())
    .bind(new.reason)
    .bind(new.outbound_sha)
    .bind(new.outbound_len.map(|n| n as i64))
    .bind(new.provider_id)
    .bind(&ts)
    .bind(&ts)
    .execute(conn)
    .await?;
    Ok(id)
}

/// Move a PENDING review to its final outcome. False when the row is not
/// pending (answered already, superseded, refused, or unknown): the caller must
/// not send on a false.
pub async fn finalize_review(
    conn: &mut SqliteConnection,
    review_id: &str,
    fin: &Finalize<'_>,
) -> Result<bool, StepReviewStoreError> {
    if !fin.outcome.is_final() {
        return Err(StepReviewStoreError::Invalid(
            "finalize needs a final outcome",
        ));
    }
    let changed = sqlx::query(
        "UPDATE step_reviews
         SET outcome = ?, total_spans = ?, auto_resolved = ?, released = ?,
             generalized = ?, removed = ?, outbound_sha = ?, outbound_len = ?,
             provider_id = COALESCE(?, provider_id), updated_at = ?
         WHERE review_id = ? AND outcome = 'pending'",
    )
    .bind(fin.outcome.as_str())
    .bind(i64::from(fin.counts.total))
    .bind(i64::from(fin.counts.auto_resolved))
    .bind(i64::from(fin.counts.released))
    .bind(i64::from(fin.counts.generalized))
    .bind(i64::from(fin.counts.removed))
    .bind(fin.outbound_sha)
    .bind(fin.outbound_len as i64)
    .bind(fin.provider_id)
    .bind(now())
    .bind(review_id)
    .execute(conn)
    .await?
    .rows_affected();
    Ok(changed == 1)
}

/// Turn a PENDING review into `blocked` with a reason code (the answers could
/// not be applied). False when it is not pending.
pub async fn block_pending_review(
    conn: &mut SqliteConnection,
    review_id: &str,
    reason: &str,
) -> Result<bool, StepReviewStoreError> {
    end_pending(conn, review_id, ReviewOutcome::Blocked, Some(reason)).await
}

/// The user refused: a PENDING review becomes `refused`. False when it is not
/// pending.
pub async fn refuse_pending_review(
    conn: &mut SqliteConnection,
    review_id: &str,
) -> Result<bool, StepReviewStoreError> {
    end_pending(conn, review_id, ReviewOutcome::Refused, None).await
}

async fn end_pending(
    conn: &mut SqliteConnection,
    review_id: &str,
    outcome: ReviewOutcome,
    reason: Option<&str>,
) -> Result<bool, StepReviewStoreError> {
    let changed = sqlx::query(
        "UPDATE step_reviews SET outcome = ?, reason = COALESCE(?, reason), updated_at = ?
         WHERE review_id = ? AND outcome = 'pending'",
    )
    .bind(outcome.as_str())
    .bind(reason)
    .bind(now())
    .bind(review_id)
    .execute(conn)
    .await?
    .rows_affected();
    Ok(changed == 1)
}

/// Supersede every PENDING review of one action of a run (a changed text starts a
/// new review). Returns the ids superseded.
pub async fn supersede_pending_for_action(
    conn: &mut SqliteConnection,
    focus_run_id: &str,
    action_id: &str,
) -> Result<Vec<String>, StepReviewStoreError> {
    let rows = sqlx::query(
        "UPDATE step_reviews SET outcome = 'superseded', updated_at = ?
         WHERE focus_run_id = ? AND action_id = ? AND outcome = 'pending'
         RETURNING review_id",
    )
    .bind(now())
    .bind(focus_run_id)
    .bind(action_id)
    .fetch_all(conn)
    .await?;
    rows.iter()
        .map(|r| r.try_get("review_id").map_err(StepReviewStoreError::from))
        .collect()
}

/// Supersede every PENDING review of a run (cancel_run). Returns the ids
/// superseded.
pub async fn supersede_pending_for_run(
    conn: &mut SqliteConnection,
    focus_run_id: &str,
) -> Result<Vec<String>, StepReviewStoreError> {
    let rows = sqlx::query(
        "UPDATE step_reviews SET outcome = 'superseded', updated_at = ?
         WHERE focus_run_id = ? AND outcome = 'pending'
         RETURNING review_id",
    )
    .bind(now())
    .bind(focus_run_id)
    .fetch_all(conn)
    .await?;
    rows.iter()
        .map(|r| r.try_get("review_id").map_err(StepReviewStoreError::from))
        .collect()
}

// ---------------------------------------------------------------------------
// Reads
// ---------------------------------------------------------------------------

const COLUMNS: &str = "review_id, focus_run_id, action_id, prompt_sha, prompt_len, spans_json,
    total_spans, auto_resolved, released, generalized, removed, outcome, reason,
    outbound_sha, outbound_len, provider_id, created_at, updated_at";

fn to_u32(v: i64) -> Result<u32, StepReviewStoreError> {
    u32::try_from(v).map_err(|_| StepReviewStoreError::Invalid("a count is out of range"))
}

fn row_of(r: &sqlx::sqlite::SqliteRow) -> Result<StepReviewRow, StepReviewStoreError> {
    let spans_json: String = r.try_get("spans_json")?;
    let spans: StoredSpans = serde_json::from_str(&spans_json)
        .map_err(|_| StepReviewStoreError::Invalid("stored spans do not parse"))?;
    let outcome_s: String = r.try_get("outcome")?;
    let outcome =
        ReviewOutcome::parse(&outcome_s).ok_or(StepReviewStoreError::Invalid("unknown outcome"))?;
    let prompt_len: i64 = r.try_get("prompt_len")?;
    let outbound_len: Option<i64> = r.try_get("outbound_len")?;
    Ok(StepReviewRow {
        review_id: r.try_get("review_id")?,
        focus_run_id: r.try_get("focus_run_id")?,
        action_id: r.try_get("action_id")?,
        prompt_sha: r.try_get("prompt_sha")?,
        prompt_len: usize::try_from(prompt_len)
            .map_err(|_| StepReviewStoreError::Invalid("a size is out of range"))?,
        spans,
        counts: Counts {
            total: to_u32(r.try_get("total_spans")?)?,
            auto_resolved: to_u32(r.try_get("auto_resolved")?)?,
            released: to_u32(r.try_get("released")?)?,
            generalized: to_u32(r.try_get("generalized")?)?,
            removed: to_u32(r.try_get("removed")?)?,
        },
        outcome,
        reason: r.try_get("reason")?,
        outbound_sha: r.try_get("outbound_sha")?,
        outbound_len: outbound_len
            .map(usize::try_from)
            .transpose()
            .map_err(|_| StepReviewStoreError::Invalid("a size is out of range"))?,
        provider_id: r.try_get("provider_id")?,
        created_at: r.try_get("created_at")?,
        updated_at: r.try_get("updated_at")?,
    })
}

/// items.id=888 stage 6: delete every review of the chat's ENDED runs (complete, failed,
/// cancelled; focus_runs.context_key = the chat), inside a write transaction the caller
/// holds. The ONE delete of this store, called only by persistence::chat_clear (the
/// archive hook; a scan pins both). Nothing reads a review once its run has ended; a run
/// that has not ended keeps its reviews, because review_prompt reuses an earlier one of
/// the same run (latest_live_review). Returns how many rows went.
pub(crate) async fn delete_reviews_of_ended_chat_runs_in_tx(
    conn: &mut SqliteConnection,
    context_key: &str,
) -> Result<u64, StepReviewStoreError> {
    let done = sqlx::query(
        "DELETE FROM step_reviews WHERE focus_run_id IN
         (SELECT f.id FROM focus_runs f
          WHERE f.context_key = ? AND f.status IN ('complete', 'failed', 'cancelled'))",
    )
    .bind(context_key)
    .execute(&mut *conn)
    .await?;
    Ok(done.rows_affected())
}

/// The newest review of one action of a run that is still LIVE: pending or
/// final. Blocked, refused and superseded rows are history and never reused.
pub async fn latest_live_review(
    conn: &mut SqliteConnection,
    focus_run_id: &str,
    action_id: &str,
) -> Result<Option<StepReviewRow>, StepReviewStoreError> {
    let sql = format!(
        "SELECT {COLUMNS} FROM step_reviews
         WHERE focus_run_id = ? AND action_id = ?
           AND outcome NOT IN ('superseded', 'blocked', 'refused')
         ORDER BY created_at DESC, rowid DESC LIMIT 1"
    );
    let row = sqlx::query(&sql)
        .bind(focus_run_id)
        .bind(action_id)
        .fetch_optional(conn)
        .await?;
    row.as_ref().map(row_of).transpose()
}

/// The newest PENDING review of a run, whatever its action.
pub async fn pending_review_for_run(
    conn: &mut SqliteConnection,
    focus_run_id: &str,
) -> Result<Option<StepReviewRow>, StepReviewStoreError> {
    let sql = format!(
        "SELECT {COLUMNS} FROM step_reviews
         WHERE focus_run_id = ? AND outcome = 'pending'
         ORDER BY created_at DESC, rowid DESC LIMIT 1"
    );
    let row = sqlx::query(&sql)
        .bind(focus_run_id)
        .fetch_optional(conn)
        .await?;
    row.as_ref().map(row_of).transpose()
}

/// Every PENDING review of the persona whose run is still open (not failed,
/// cancelled or complete), oldest first. A run that has ended has no one to answer
/// for: cancel_run supersedes its pending rows, and a run that ended any other way
/// is left out here rather than shown. The persona is the file, so this is the
/// persona's.
pub async fn list_open_pending_reviews(
    conn: &mut SqliteConnection,
) -> Result<Vec<StepReviewRow>, StepReviewStoreError> {
    let columns = COLUMNS
        .split(',')
        .map(|c| format!("r.{}", c.trim()))
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        "SELECT {columns} FROM step_reviews r
         JOIN focus_runs f ON f.id = r.focus_run_id
         WHERE r.outcome = 'pending'
           AND f.status NOT IN ('failed', 'cancelled', 'complete')
         ORDER BY r.created_at ASC, r.rowid ASC"
    );
    let rows = sqlx::query(&sql).fetch_all(conn).await?;
    rows.iter().map(row_of).collect()
}

/// Every review of one action of a run, oldest first (for items.id=901 and 913).
pub async fn list_reviews(
    conn: &mut SqliteConnection,
    focus_run_id: &str,
    action_id: &str,
) -> Result<Vec<StepReviewRow>, StepReviewStoreError> {
    let sql = format!(
        "SELECT {COLUMNS} FROM step_reviews
         WHERE focus_run_id = ? AND action_id = ?
         ORDER BY created_at ASC, rowid ASC"
    );
    let rows = sqlx::query(&sql)
        .bind(focus_run_id)
        .bind(action_id)
        .fetch_all(conn)
        .await?;
    rows.iter().map(row_of).collect()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persistence::session_store::test_db::mem_outputs_conn;

    const SHA_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const SHA_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    async fn conn_with_run(run: &str) -> SqliteConnection {
        let mut conn = mem_outputs_conn().await;
        sqlx::query("INSERT INTO focus_runs (id, focus_id, started_at) VALUES (?, 'f', 't')")
            .bind(run)
            .execute(&mut conn)
            .await
            .unwrap();
        conn
    }

    fn spans() -> StoredSpans {
        StoredSpans {
            asked: vec![AskedSpan {
                span_id: "s1".into(),
                start_byte: 4,
                end_byte: 7,
                category: "private_person".into(),
                review_tier: "high".into(),
            }],
            settled: vec![SettledSpan {
                start_byte: 10,
                end_byte: 14,
                decision: "generalize".into(),
                suggestion_text: Some("[person]".into()),
                user_modified_text: None,
            }],
        }
    }

    fn pending<'a>(
        run: &'a str,
        action: &'a str,
        sha: &'a str,
        s: &'a StoredSpans,
    ) -> NewReview<'a> {
        NewReview {
            focus_run_id: run,
            action_id: action,
            prompt_sha: sha,
            prompt_len: 20,
            spans: s,
            counts: Counts {
                total: 2,
                auto_resolved: 1,
                ..Counts::default()
            },
            outcome: ReviewOutcome::Pending,
            reason: None,
            outbound_sha: None,
            outbound_len: None,
            provider_id: Some("prov"),
        }
    }

    fn fin(outcome: ReviewOutcome) -> Finalize<'static> {
        Finalize {
            outcome,
            counts: Counts {
                total: 2,
                auto_resolved: 1,
                released: 1,
                generalized: 1,
                removed: 0,
            },
            outbound_sha: SHA_B,
            outbound_len: 18,
            provider_id: None,
        }
    }

    #[test]
    fn the_outcomes_are_the_closed_set_and_round_trip() {
        let names: Vec<&str> = ReviewOutcome::ALL.iter().map(|o| o.as_str()).collect();
        assert_eq!(
            names,
            [
                "clean",
                "standing_applied",
                "approved_as_shown",
                "edited",
                "redacted",
                "refused",
                "blocked",
                "pending",
                "superseded"
            ]
        );
        for o in ReviewOutcome::ALL {
            assert_eq!(ReviewOutcome::parse(o.as_str()), Some(o));
        }
        assert_eq!(ReviewOutcome::parse("Clean"), None);
        assert_eq!(ReviewOutcome::parse("applied"), None);
        let finals: Vec<_> = ReviewOutcome::ALL.iter().filter(|o| o.is_final()).collect();
        assert_eq!(finals.len(), 5, "five outcomes release text");
    }

    #[tokio::test]
    async fn a_pending_review_round_trips_and_is_finalised_once() {
        let mut conn = conn_with_run("run1").await;
        let s = spans();
        let id = insert_review(&mut conn, &pending("run1", "act", SHA_A, &s))
            .await
            .unwrap();

        let live = latest_live_review(&mut conn, "run1", "act")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(live.review_id, id);
        assert_eq!(live.outcome, ReviewOutcome::Pending);
        assert_eq!(live.spans, s);
        assert_eq!(live.counts.total, 2);
        assert_eq!(live.provider_id.as_deref(), Some("prov"));
        assert!(live.outbound_sha.is_none());
        assert_eq!(
            pending_review_for_run(&mut conn, "run1")
                .await
                .unwrap()
                .unwrap()
                .review_id,
            id
        );

        assert!(finalize_review(&mut conn, &id, &fin(ReviewOutcome::Edited))
            .await
            .unwrap());
        // Answered once: a second finalize, a refuse and a block all do nothing.
        assert!(
            !finalize_review(&mut conn, &id, &fin(ReviewOutcome::Redacted))
                .await
                .unwrap()
        );
        assert!(!refuse_pending_review(&mut conn, &id).await.unwrap());
        assert!(!block_pending_review(&mut conn, &id, "x").await.unwrap());

        let done = latest_live_review(&mut conn, "run1", "act")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(done.outcome, ReviewOutcome::Edited);
        assert_eq!(done.outbound_sha.as_deref(), Some(SHA_B));
        assert_eq!(done.outbound_len, Some(18));
        assert_eq!(done.counts.released, 1);
        assert_eq!(
            done.provider_id.as_deref(),
            Some("prov"),
            "kept when not given"
        );
        assert!(pending_review_for_run(&mut conn, "run1")
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn only_a_final_outcome_carries_an_outbound_hash() {
        let mut conn = conn_with_run("run1").await;
        let s = StoredSpans::default();
        let mut bad = pending("run1", "act", SHA_A, &s);
        bad.outbound_sha = Some(SHA_B);
        bad.outbound_len = Some(1);
        assert!(insert_review(&mut conn, &bad).await.is_err());

        let mut bad = pending("run1", "act", SHA_A, &s);
        bad.outcome = ReviewOutcome::Clean;
        assert!(
            insert_review(&mut conn, &bad).await.is_err(),
            "final needs a hash"
        );

        let mut ok = pending("run1", "act", SHA_A, &s);
        ok.outcome = ReviewOutcome::Clean;
        ok.outbound_sha = Some(SHA_A);
        ok.outbound_len = Some(20);
        insert_review(&mut conn, &ok).await.unwrap();

        assert!(
            finalize_review(&mut conn, "nope", &fin(ReviewOutcome::Blocked))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn blocked_refused_and_superseded_rows_are_history_not_live() {
        let mut conn = conn_with_run("run1").await;
        let s = StoredSpans::default();
        let a = insert_review(&mut conn, &pending("run1", "act", SHA_A, &s))
            .await
            .unwrap();
        assert!(block_pending_review(&mut conn, &a, "unappliable")
            .await
            .unwrap());
        assert!(latest_live_review(&mut conn, "run1", "act")
            .await
            .unwrap()
            .is_none());

        let b = insert_review(&mut conn, &pending("run1", "act", SHA_A, &s))
            .await
            .unwrap();
        assert!(refuse_pending_review(&mut conn, &b).await.unwrap());
        assert!(latest_live_review(&mut conn, "run1", "act")
            .await
            .unwrap()
            .is_none());

        let c = insert_review(&mut conn, &pending("run1", "act", SHA_A, &s))
            .await
            .unwrap();
        assert_eq!(
            supersede_pending_for_action(&mut conn, "run1", "act")
                .await
                .unwrap(),
            std::slice::from_ref(&c)
        );
        assert!(latest_live_review(&mut conn, "run1", "act")
            .await
            .unwrap()
            .is_none());

        let all = list_reviews(&mut conn, "run1", "act").await.unwrap();
        let outcomes: Vec<_> = all.iter().map(|r| r.outcome).collect();
        assert_eq!(
            outcomes,
            [
                ReviewOutcome::Blocked,
                ReviewOutcome::Refused,
                ReviewOutcome::Superseded
            ]
        );
        assert_eq!(all[0].reason.as_deref(), Some("unappliable"));
        assert_eq!(all[0].review_id, a);
        assert_eq!(all[2].review_id, c);
        let _ = b;
    }

    #[tokio::test]
    async fn supersede_by_run_and_by_action_touch_only_pending_rows() {
        let mut conn = conn_with_run("run1").await;
        sqlx::query("INSERT INTO focus_runs (id, focus_id, started_at) VALUES ('run2', 'f', 't')")
            .execute(&mut conn)
            .await
            .unwrap();
        let s = StoredSpans::default();
        insert_review(&mut conn, &pending("run1", "a", SHA_A, &s))
            .await
            .unwrap();
        insert_review(&mut conn, &pending("run1", "b", SHA_A, &s))
            .await
            .unwrap();
        insert_review(&mut conn, &pending("run2", "a", SHA_A, &s))
            .await
            .unwrap();
        let mut done = pending("run1", "c", SHA_A, &s);
        done.outcome = ReviewOutcome::Clean;
        done.outbound_sha = Some(SHA_A);
        done.outbound_len = Some(20);
        insert_review(&mut conn, &done).await.unwrap();

        assert_eq!(
            supersede_pending_for_action(&mut conn, "run1", "a")
                .await
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            supersede_pending_for_run(&mut conn, "run1")
                .await
                .unwrap()
                .len(),
            1
        );
        assert!(pending_review_for_run(&mut conn, "run1")
            .await
            .unwrap()
            .is_none());
        assert!(pending_review_for_run(&mut conn, "run2")
            .await
            .unwrap()
            .is_some());
        assert_eq!(
            latest_live_review(&mut conn, "run1", "c")
                .await
                .unwrap()
                .unwrap()
                .outcome,
            ReviewOutcome::Clean,
            "a final row is never superseded"
        );
    }

    #[tokio::test]
    async fn the_open_pending_list_holds_only_pending_rows_of_open_runs() {
        let mut conn = conn_with_run("run1").await;
        for (id, status) in [
            ("run2", "awaiting_user"),
            ("run3", "cancelled"),
            ("run4", "failed"),
            ("run5", "complete"),
        ] {
            sqlx::query(
                "INSERT INTO focus_runs (id, focus_id, status, started_at) VALUES (?, 'f', ?, 't')",
            )
            .bind(id)
            .bind(status)
            .execute(&mut conn)
            .await
            .unwrap();
        }
        let s = StoredSpans::default();
        let first = insert_review(&mut conn, &pending("run1", "a", SHA_A, &s))
            .await
            .unwrap();
        let second = insert_review(&mut conn, &pending("run2", "b", SHA_A, &s))
            .await
            .unwrap();
        for run in ["run3", "run4", "run5"] {
            insert_review(&mut conn, &pending(run, "a", SHA_A, &s))
                .await
                .unwrap();
        }
        // A final row and a blocked row of an open run are not pending.
        let mut done = pending("run1", "c", SHA_A, &s);
        done.outcome = ReviewOutcome::Clean;
        done.outbound_sha = Some(SHA_A);
        done.outbound_len = Some(20);
        insert_review(&mut conn, &done).await.unwrap();
        let blocked = insert_review(&mut conn, &pending("run2", "d", SHA_A, &s))
            .await
            .unwrap();
        assert!(block_pending_review(&mut conn, &blocked, "x")
            .await
            .unwrap());

        let listed = list_open_pending_reviews(&mut conn).await.unwrap();
        let ids: Vec<&str> = listed.iter().map(|r| r.review_id.as_str()).collect();
        assert_eq!(ids, [first.as_str(), second.as_str()], "oldest first");
        assert_eq!(listed[1].focus_run_id, "run2");
    }

    #[tokio::test]
    async fn the_foreign_key_and_the_size_checks_hold() {
        let mut conn = mem_outputs_conn().await;
        let s = StoredSpans::default();
        assert!(
            insert_review(&mut conn, &pending("no-such-run", "act", SHA_A, &s))
                .await
                .is_err(),
            "focus_run_id references focus_runs"
        );
        let mut conn = conn_with_run("run1").await;
        assert!(
            insert_review(&mut conn, &pending("run1", "act", "short", &s))
                .await
                .is_err(),
            "prompt_sha is a 64 character hash"
        );
    }

    /// The columns a review may hold, pinned so a text column cannot be added
    /// without a decision.
    #[tokio::test]
    async fn the_table_has_no_prompt_text_column() {
        let mut conn = mem_outputs_conn().await;
        let cols: Vec<(String,)> =
            sqlx::query_as("SELECT name FROM pragma_table_info('step_reviews') ORDER BY cid")
                .fetch_all(&mut conn)
                .await
                .unwrap();
        let names: Vec<&str> = cols.iter().map(|c| c.0.as_str()).collect();
        assert_eq!(
            names,
            [
                "review_id",
                "focus_run_id",
                "action_id",
                "prompt_sha",
                "prompt_len",
                "spans_json",
                "total_spans",
                "auto_resolved",
                "released",
                "generalized",
                "removed",
                "outcome",
                "reason",
                "outbound_sha",
                "outbound_len",
                "provider_id",
                "created_at",
                "updated_at"
            ]
        );
        let triggers: Vec<(String,)> = sqlx::query_as(
            "SELECT name FROM sqlite_master WHERE type = 'trigger' AND tbl_name = 'step_reviews'",
        )
        .fetch_all(&mut conn)
        .await
        .unwrap();
        assert!(triggers.is_empty());
    }

    #[test]
    fn the_module_logs_nothing_and_has_no_query_macros() {
        let src = include_str!("step_review_store.rs");
        let code = src.split("#[cfg(test)]").next().unwrap();
        for banned in [
            "log::",
            "println!",
            "eprintln!",
            "dbg!",
            "tracing::",
            "sqlx::query!(",
            "query_as!(",
            "query_scalar!(",
        ] {
            assert!(!code.contains(banned), "step_review_store uses {banned}");
        }
        // items.id=888 stage 6 delta: one delete, delete_reviews_of_ended_chat_runs_in_tx,
        // the archive hook's (reviews of a chat's ENDED runs only; nothing reads a review
        // once its run has ended). persistence::chat_clear's tests pin its single caller.
        assert_eq!(
            code.matches("DELETE FROM").count(),
            1,
            "this store's one delete is the archive hook's"
        );
    }
}
