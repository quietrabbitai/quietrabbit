// src-tauri/src/persistence/shared_log_read.rs
//
// items.id=957 unit 1 (decisions.id=967, 434, 944): the read path behind the
// "What QR shared" view. READ ONLY: this module has no INSERT, UPDATE or DELETE
// (a source-scan test pins it). disclosure_log is never deleted (personal_001.sql).
//
// PRIVACY SHAPE (the gate-critical part). What may leave this module is counts,
// event-type strings, provider ids, run ids, Focus ids, dates and FIELD NAMES.
// It never returns a field VALUE:
//   - fields_abstracted holds name -> abstracted value. It is read ONLY through
//     json_each(...).key, so the values are never selected (a source-scan test
//     pins every mention, and a sentinel test proves a value never surfaces).
//   - content_summary of a consent row is a FIXED description by decision
//     (decisions.id=434, 944), safe to display, and is the only free text here.
//   - the event_type strings stay inside the backend; the command layer turns
//     them into sentences (unit 2) and never forwards a raw string.
//
// THREE-STEP READ, no ATTACH (the files are separate encrypted databases and the
// topology opens one connection per file, database.md):
//   1. personal.db: ONE statement over disclosure_log, in two parts (items.id=958
//      unit 2). Events: rows grouped by (day, run, event type, provider, declined),
//      except the rows that make a send. Actions: one row per (run, action) with
//      flags (a gate1_pass, an approval, a review mark, a held row), so one send
//      that wrote several rows is counted once. The view loads from this summary
//      and never pages every row. Tier-1 rows are excluded (nothing left the
//      computer).
//   2. outputs.db: focus_runs.id -> focus_id for the distinct runs of step 1,
//      so the cost is bounded by runs, not log rows.
//   3. shared.db: provider display name and mode, and the Focus display name,
//      each looked up once per distinct id.
//
// DAYS are computed in the user's local time: the caller passes the UTC offset in
// minutes, so a send at 11pm local lands on that local day.

use std::collections::HashMap;

use sqlx::{Row, SqliteConnection};
use thiserror::Error;

use crate::persistence::provider_store::ProviderMode;

#[derive(Debug, Error)]
pub enum SharedLogError {
    #[error("database error: {0}")]
    Sql(#[from] sqlx::Error),
    #[error("could not open the Persona's data: {0}")]
    Open(String),
}

/// Most run ids per `IN (...)` lookup.
const RUN_CHUNK: usize = 500;
/// A real UTC offset is within this many minutes either side.
const MAX_OFFSET_MINUTES: i32 = 14 * 60;

/// One group of disclosure_log rows: the same local day, run, event type,
/// provider and declined flag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisclosureAggregate {
    pub day: String,
    pub focus_run_id: String,
    /// The writer's event string, "" for a row with none. Never forwarded to the
    /// frontend.
    pub event_type: String,
    pub provider: Option<String>,
    pub override_declined: bool,
    pub rows: i64,
    /// Fields shared as they were plus fields shared in a general form.
    pub shared: i64,
    pub withheld: i64,
}

/// One action (run, action_id) that wrote at least one row that can make a send, a
/// hold or a review mark. Flags and counts only: no field name, no value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionAggregate {
    /// The local day of the action's newest sending or held row.
    pub day: String,
    pub focus_run_id: String,
    pub action_id: String,
    /// The action's non-null provider (None when no row names one).
    pub provider: Option<String>,
    /// Fields withheld, from its gate1_pass row only (the most over retries).
    pub withheld: i64,
    pub gate1: bool,
    /// A gate3_promotion_approved or gate3_pf_no_spans row.
    pub approved: bool,
    /// A promotion, a pending consent or a reapplied choice.
    pub marked: bool,
    /// A held gate3 row.
    pub held: bool,
}

/// What the one statement of step 1 returns.
#[derive(Debug, Default)]
pub struct DisclosureAggregates {
    pub events: Vec<DisclosureAggregate>,
    pub actions: Vec<ActionAggregate>,
}

/// One consent_audit_records row, without anything but its fixed summary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsentRowView {
    pub id: String,
    pub day: String,
    pub created_at: String,
    /// The stored string. A string this version does not know stays a string so
    /// the row still appears under the fallback sentence.
    pub event_type: String,
    pub focus_slug: String,
    pub has_reference: bool,
    pub document_type: Option<String>,
    pub content_summary: String,
}

/// One disclosure row opened for the second view: names only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisclosureDetailRow {
    pub focus_run_id: String,
    pub action_id: String,
    /// The row's own local day.
    pub day: String,
    pub created_at: String,
    pub event_type: String,
    pub provider: Option<String>,
    pub override_declined: bool,
    /// Shared as they were, then shared in a general form. Names, never values.
    pub shared: Vec<String>,
    pub withheld: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderInfo {
    pub display_name: String,
    pub mode: ProviderMode,
}

/// What steps 1 and 2 return together.
#[derive(Debug, Default)]
pub struct SharedLogRaw {
    /// Event rows, the ones that make a send excluded.
    pub aggregates: Vec<DisclosureAggregate>,
    /// One entry per action that can be a send, a hold or a review mark.
    pub actions: Vec<ActionAggregate>,
    pub consent: Vec<ConsentRowView>,
    /// focus_run_id -> focus_id. A run with no focus_runs row is absent.
    pub run_focus: HashMap<String, String>,
}

fn offset_modifier(offset_minutes: i32) -> String {
    let m = offset_minutes.clamp(-MAX_OFFSET_MINUTES, MAX_OFFSET_MINUTES);
    format!("{m:+} minutes")
}

/// The shared tail of the two parts of step 1: the same field counts as before,
/// computed once per row in a materialized CTE.
const ROW_COLUMNS: &str = "date(created_at, ?1) AS day, focus_run_id, action_id,
                CASE WHEN json_valid(extra_metadata)
                     THEN COALESCE(json_extract(extra_metadata, '$.event_type'), '')
                     ELSE '' END AS ev,
                provider, override_declined,
                CASE WHEN json_valid(fields_shared)
                     THEN json_array_length(fields_shared) ELSE 0 END
                + CASE WHEN json_valid(fields_abstracted)
                       THEN (SELECT COUNT(*) FROM json_each(fields_abstracted))
                       ELSE 0 END AS shared_n,
                CASE WHEN json_valid(fields_withheld)
                     THEN json_array_length(fields_withheld) ELSE 0 END AS withheld_n";

/// Step 1a. ONE statement, tier-1 rows excluded, `since` an RFC 3339 lower bound on
/// created_at (None = everything). Two parts over one pass of the rows (items.id=958
/// unit 2):
///   - events: every row that does not make a send, grouped as before. Held rows and
///     review marks are here too, so they are still counted and shown one by one.
///   - actions: one row per (run, action) holding flags, so the several rows one send
///     writes (a gate1_pass per attempt, the review's gate3 row) count once. The
///     day is the local day of its newest sending or held row; withheld is the most
///     any one gate1_pass row withheld, and a row with no gate1_pass withheld none.
pub async fn disclosure_aggregates(
    conn: &mut SqliteConnection,
    since: Option<&str>,
    offset_minutes: i32,
) -> Result<DisclosureAggregates, SharedLogError> {
    use crate::commands::shared_log_deck::{
        sql_list, HOLD_EVENTS, REVIEW_MARK_EVENTS, SEND_EVENTS,
    };
    let sends = sql_list(&SEND_EVENTS);
    let holds = sql_list(&HOLD_EVENTS);
    let marks = sql_list(&REVIEW_MARK_EVENTS);
    let approvals = sql_list(&["gate3_promotion_approved", "gate3_pf_no_spans"]);
    let sql = format!(
        "WITH t AS MATERIALIZED (
             SELECT {ROW_COLUMNS}
             FROM disclosure_log
             WHERE routing_tier > 1
               AND (?2 IS NULL OR created_at >= ?2))
         SELECT 'E' AS kind, day, focus_run_id, '' AS action_id, ev AS event_type, provider,
                override_declined, COUNT(*) AS n, SUM(shared_n) AS shared_n,
                SUM(withheld_n) AS withheld_n, 0 AS gate1, 0 AS approved, 0 AS marked,
                0 AS held
           FROM t
          WHERE ev NOT IN {sends}
          GROUP BY day, focus_run_id, ev, provider, override_declined
         UNION ALL
         SELECT 'A', MAX(CASE WHEN ev IN {sends} OR ev IN {holds} THEN day END),
                focus_run_id, action_id, '', MAX(provider), 0, 1, 0,
                COALESCE(MAX(CASE WHEN ev = 'gate1_pass' THEN withheld_n END), 0),
                MAX(ev = 'gate1_pass'), MAX(ev IN {approvals}), MAX(ev IN {marks}),
                MAX(ev IN {holds})
           FROM t
          WHERE ev IN {sends} OR ev IN {holds} OR ev IN {marks}
          GROUP BY focus_run_id, action_id"
    );
    let rows = sqlx::query(&sql)
        .bind(offset_modifier(offset_minutes))
        .bind(since)
        .fetch_all(&mut *conn)
        .await?;
    let mut out = DisclosureAggregates::default();
    for r in &rows {
        let kind: String = r.try_get("kind")?;
        let day = r.try_get::<Option<String>, _>("day")?.unwrap_or_default();
        if kind == "E" {
            out.events.push(DisclosureAggregate {
                day,
                focus_run_id: r.try_get("focus_run_id")?,
                event_type: r.try_get("event_type")?,
                provider: r.try_get("provider")?,
                override_declined: r.try_get::<i64, _>("override_declined")? != 0,
                rows: r.try_get("n")?,
                shared: r.try_get("shared_n")?,
                withheld: r.try_get("withheld_n")?,
            });
        } else if !day.is_empty() {
            // An action with only review marks and no sending or held row has no day
            // and is nothing: a mark alone is not a send.
            out.actions.push(ActionAggregate {
                day,
                focus_run_id: r.try_get("focus_run_id")?,
                action_id: r.try_get("action_id")?,
                provider: r.try_get("provider")?,
                withheld: r.try_get("withheld_n")?,
                gate1: r.try_get::<i64, _>("gate1")? != 0,
                approved: r.try_get::<i64, _>("approved")? != 0,
                marked: r.try_get::<i64, _>("marked")? != 0,
                held: r.try_get::<i64, _>("held")? != 0,
            });
        }
    }
    out.events.sort_by(|a, b| {
        b.day
            .cmp(&a.day)
            .then_with(|| a.focus_run_id.cmp(&b.focus_run_id))
    });
    out.actions.sort_by(|a, b| {
        b.day
            .cmp(&a.day)
            .then_with(|| a.focus_run_id.cmp(&b.focus_run_id))
    });
    Ok(out)
}

/// Step 1b. Every consent record in the window, oldest first. The table is small
/// (one row per choice), so it is read directly.
pub async fn consent_rows(
    conn: &mut SqliteConnection,
    since: Option<&str>,
    offset_minutes: i32,
) -> Result<Vec<ConsentRowView>, SharedLogError> {
    let rows = sqlx::query(
        "SELECT id, date(created_at, ?1) AS day, created_at, event_type, focus_slug,
                consent_reference_id IS NOT NULL AS has_ref, document_type, content_summary
         FROM consent_audit_records
         WHERE (?2 IS NULL OR created_at >= ?2)
         ORDER BY created_at ASC, rowid ASC",
    )
    .bind(offset_modifier(offset_minutes))
    .bind(since)
    .fetch_all(&mut *conn)
    .await?;
    rows.iter()
        .map(|r| {
            Ok(ConsentRowView {
                id: r.try_get("id")?,
                day: r.try_get::<Option<String>, _>("day")?.unwrap_or_default(),
                created_at: r.try_get("created_at")?,
                event_type: r.try_get("event_type")?,
                focus_slug: r.try_get("focus_slug")?,
                has_reference: r.try_get::<i64, _>("has_ref")? != 0,
                document_type: r.try_get("document_type")?,
                content_summary: r.try_get("content_summary")?,
            })
        })
        .collect()
}

/// Step 2. focus_runs.id -> focus_id for these runs (outputs.db), in chunks.
pub async fn run_focus_map(
    conn: &mut SqliteConnection,
    run_ids: &[String],
) -> Result<HashMap<String, String>, SharedLogError> {
    let mut out = HashMap::new();
    for chunk in run_ids.chunks(RUN_CHUNK) {
        if chunk.is_empty() {
            continue;
        }
        let marks = vec!["?"; chunk.len()].join(",");
        let sql = format!("SELECT id, focus_id FROM focus_runs WHERE id IN ({marks})");
        let mut q = sqlx::query(&sql);
        for id in chunk {
            q = q.bind(id);
        }
        for r in q.fetch_all(&mut *conn).await? {
            out.insert(r.try_get("id")?, r.try_get("focus_id")?);
        }
    }
    Ok(out)
}

/// The second open: every row of the given runs (each with its own local day), with
/// field NAMES only. The caller keeps the rows of the day it opened and folds the
/// rows of one action into one send, as step 1 does. fields_abstracted contributes
/// its keys through json_each and nothing else.
pub async fn disclosure_details(
    conn: &mut SqliteConnection,
    run_ids: &[String],
    offset_minutes: i32,
) -> Result<Vec<DisclosureDetailRow>, SharedLogError> {
    let mut out = Vec::new();
    for chunk in run_ids.chunks(RUN_CHUNK) {
        if chunk.is_empty() {
            continue;
        }
        let marks = vec!["?"; chunk.len()].join(",");
        let sql = format!(
            "SELECT focus_run_id, action_id, date(created_at, ?) AS day, created_at,
                    CASE WHEN json_valid(extra_metadata)
                         THEN COALESCE(json_extract(extra_metadata, '$.event_type'), '')
                         ELSE '' END AS event_type,
                    provider, override_declined,
                    CASE WHEN json_valid(fields_shared) THEN fields_shared ELSE '[]' END
                        AS shared_names,
                    CASE WHEN json_valid(fields_abstracted)
                         THEN (SELECT COALESCE(json_group_array(key), '[]')
                               FROM json_each(fields_abstracted))
                         ELSE '[]' END AS abstracted_names,
                    CASE WHEN json_valid(fields_withheld) THEN fields_withheld ELSE '[]' END
                        AS withheld_names
             FROM disclosure_log
             WHERE routing_tier > 1
               AND focus_run_id IN ({marks})
             ORDER BY created_at ASC, rowid ASC"
        );
        let mut q = sqlx::query(&sql).bind(offset_modifier(offset_minutes));
        for id in chunk {
            q = q.bind(id);
        }
        for r in q.fetch_all(&mut *conn).await? {
            let mut shared = names(&r.try_get::<String, _>("shared_names")?);
            shared.extend(names(&r.try_get::<String, _>("abstracted_names")?));
            out.push(DisclosureDetailRow {
                focus_run_id: r.try_get("focus_run_id")?,
                action_id: r.try_get("action_id")?,
                day: r.try_get::<Option<String>, _>("day")?.unwrap_or_default(),
                created_at: r.try_get("created_at")?,
                event_type: r.try_get("event_type")?,
                provider: r.try_get("provider")?,
                override_declined: r.try_get::<i64, _>("override_declined")? != 0,
                shared,
                withheld: names(&r.try_get::<String, _>("withheld_names")?),
            });
        }
    }
    Ok(out)
}

/// A JSON array of strings, or empty for anything else.
fn names(json: &str) -> Vec<String> {
    serde_json::from_str::<Vec<String>>(json).unwrap_or_default()
}

/// Steps 1 and 2 against the Persona's own files (each opened separately,
/// decisions.id=944). The Persona is whichever pair of ids the caller has already
/// checked belongs to the signed-in user.
pub async fn load_raw(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
    since: Option<&str>,
    offset_minutes: i32,
) -> Result<SharedLogRaw, SharedLogError> {
    let mut personal =
        crate::persistence::personal_store::open_personal_db(user_id, persona_id, key_hex)
            .await
            .map_err(|e| SharedLogError::Open(e.to_string()))?;
    let DisclosureAggregates {
        events: aggregates,
        actions,
    } = disclosure_aggregates(&mut personal, since, offset_minutes).await?;
    let consent = consent_rows(&mut personal, since, offset_minutes).await?;
    drop(personal);

    let mut run_ids: Vec<String> = aggregates
        .iter()
        .map(|a| a.focus_run_id.clone())
        .chain(actions.iter().map(|a| a.focus_run_id.clone()))
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    run_ids.retain(|r| !r.is_empty());
    let mut outputs =
        crate::persistence::output_store::open_outputs_db(user_id, persona_id, key_hex)
            .await
            .map_err(|e| SharedLogError::Open(e.to_string()))?;
    let run_focus = run_focus_map(&mut outputs, &run_ids).await?;
    Ok(SharedLogRaw {
        aggregates,
        actions,
        consent,
        run_focus,
    })
}

/// Step 3a. Provider display name and mode from shared.db, once per id. An id
/// with no row is absent: the caller falls back to "an outside service".
pub async fn provider_infos(
    pool: &sqlx::SqlitePool,
    ids: &[String],
) -> Result<HashMap<String, ProviderInfo>, SharedLogError> {
    let mut out = HashMap::new();
    for id in ids {
        if out.contains_key(id) {
            continue;
        }
        if let Some(p) = crate::persistence::provider_store::get_provider(pool, id)
            .await
            .map_err(|e| SharedLogError::Open(e.to_string()))?
        {
            out.insert(
                id.clone(),
                ProviderInfo {
                    display_name: p.display_name,
                    mode: p.mode,
                },
            );
        }
    }
    Ok(out)
}

/// Step 3b. Focus display names from shared.db. A Focus with no row is absent.
pub async fn focus_names(
    pool: &sqlx::SqlitePool,
    ids: &[String],
) -> Result<HashMap<String, String>, SharedLogError> {
    let mut out = HashMap::new();
    for id in ids {
        if out.contains_key(id) {
            continue;
        }
        if let Some(f) = crate::persistence::focus_composition_store::get_focus(pool, id).await? {
            out.insert(id.clone(), f.display_name);
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persistence::migrations::parse_statements;
    use sqlx::sqlite::SqliteConnectOptions;
    use sqlx::ConnectOptions;

    const SENTINEL: &str = "SENTINEL-ABSTRACTED-VALUE-7731";

    async fn mem(schemas: &[&str]) -> SqliteConnection {
        let mut conn = SqliteConnectOptions::new()
            .filename(":memory:")
            .connect()
            .await
            .expect("in-memory connection failed");
        for schema in schemas {
            for stmt in parse_statements(schema) {
                sqlx::query(&stmt)
                    .execute(&mut conn)
                    .await
                    .unwrap_or_else(|e| panic!("schema statement failed: {e}\n{stmt}"));
            }
        }
        conn
    }

    async fn personal() -> SqliteConnection {
        mem(&[
            include_str!("../../schema/personal_001.sql"),
            include_str!("../../schema/personal_008.sql"),
            include_str!("../../schema/personal_010.sql"),
        ])
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn put(
        conn: &mut SqliteConnection,
        at: &str,
        run: &str,
        tier: i64,
        event: &str,
        provider: Option<&str>,
        shared: &str,
        abstracted: &str,
        withheld: &str,
        declined: i64,
    ) {
        put_act(
            conn, "a", at, run, tier, event, provider, shared, abstracted, withheld, declined,
        )
        .await;
    }

    #[allow(clippy::too_many_arguments)]
    async fn put_act(
        conn: &mut SqliteConnection,
        action: &str,
        at: &str,
        run: &str,
        tier: i64,
        event: &str,
        provider: Option<&str>,
        shared: &str,
        abstracted: &str,
        withheld: &str,
        declined: i64,
    ) {
        sqlx::query(
            "INSERT INTO disclosure_log
             (id, user_id, persona_id, focus_run_id, action_id, routing_tier, provider,
              fields_shared, fields_abstracted, fields_withheld, override_declined,
              created_at, extra_metadata, execution_tier)
             VALUES (?, 'u', 'p', ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(uuid::Uuid::new_v4().to_string())
        .bind(run)
        .bind(action)
        .bind(tier)
        .bind(provider)
        .bind(shared)
        .bind(abstracted)
        .bind(withheld)
        .bind(declined)
        .bind(at)
        .bind(format!("{{\"event_type\":\"{event}\"}}"))
        .bind(tier)
        .execute(conn)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn aggregates_group_by_local_day_and_exclude_tier_one() {
        let mut c = personal().await;
        put(
            &mut c,
            "2026-11-20T10:00:00+00:00",
            "r1",
            2,
            "gate1_pass",
            Some("p1"),
            "[\"job_title\"]",
            &format!("{{\"employer\":\"{SENTINEL}\"}}"),
            "[\"location_city\"]",
            0,
        )
        .await;
        // A retry of the same action: one send, the most it withheld.
        put(
            &mut c,
            "2026-11-20T10:05:00+00:00",
            "r1",
            2,
            "gate1_pass",
            Some("p1"),
            "[]",
            "{}",
            "[]",
            0,
        )
        .await;
        // A tier-1 row of another action: nothing left the computer.
        put_act(
            &mut c,
            "a-local",
            "2026-11-20T10:06:00+00:00",
            "r1",
            1,
            "gate1_pass",
            Some("local"),
            "[\"city\"]",
            "{}",
            "[]",
            0,
        )
        .await;
        put(
            &mut c,
            "2026-11-21T01:00:00+00:00",
            "r2",
            2,
            "gate3_pf_no_spans",
            None,
            "[\"draft\"]",
            "{}",
            "[]",
            0,
        )
        .await;
        put_act(
            &mut c,
            "a-hold",
            "2026-11-20T10:07:00+00:00",
            "r1",
            2,
            "gate3_sensitivity_block",
            None,
            "[]",
            "{}",
            "[\"draft\"]",
            1,
        )
        .await;

        let utc = disclosure_aggregates(&mut c, None, 0).await.unwrap();
        // Two sending actions (the two p1 rows are ONE), no tier-1 action. The held row
        // is an event row AND flags its own action.
        assert_eq!(utc.events.len(), 1);
        assert_eq!(utc.events[0].event_type, "gate3_sensitivity_block");
        assert_eq!(utc.actions.len(), 3, "{:?}", utc.actions);
        let g = utc
            .actions
            .iter()
            .find(|a| a.action_id == "a" && a.focus_run_id == "r1")
            .unwrap();
        assert_eq!(
            (g.gate1, g.approved, g.marked, g.held),
            (true, false, false, false)
        );
        assert_eq!(
            g.withheld, 1,
            "the most a gate1_pass row withheld, not the sum"
        );
        assert_eq!(
            (g.day.as_str(), g.provider.as_deref()),
            ("2026-11-20", Some("p1"))
        );
        let hold = utc
            .actions
            .iter()
            .find(|a| a.action_id == "a-hold")
            .unwrap();
        assert!(hold.held && !hold.gate1 && !hold.approved);
        let r2 = utc.actions.iter().find(|a| a.focus_run_id == "r2").unwrap();
        assert!(r2.approved && !r2.gate1 && r2.withheld == 0 && r2.provider.is_none());
        // The 01:00 UTC row falls on the PREVIOUS local day at UTC-8.
        let local = disclosure_aggregates(&mut c, None, -480).await.unwrap();
        assert_eq!(
            local
                .actions
                .iter()
                .find(|a| a.focus_run_id == "r2")
                .unwrap()
                .day,
            "2026-11-20"
        );
    }

    /// A review mark with no sending or held row is no action at all (it has no day).
    #[tokio::test]
    async fn a_mark_alone_is_not_an_action_and_stays_an_event() {
        let mut c = personal().await;
        put(
            &mut c,
            "2026-11-20T10:00:00+00:00",
            "r1",
            2,
            "gate3_consent_pending",
            Some("p"),
            "[]",
            "{}",
            "[\"draft\"]",
            0,
        )
        .await;
        let a = disclosure_aggregates(&mut c, None, 0).await.unwrap();
        assert!(a.actions.is_empty());
        assert_eq!(a.events.len(), 1);
    }

    /// The action's flags and day when its rows span a day, a retry and a review.
    #[tokio::test]
    async fn an_action_spanning_days_is_one_row_on_its_newest_sending_row() {
        let mut c = personal().await;
        for (at, ev) in [
            ("2026-11-20T23:00:00+00:00", "gate1_pass"),
            ("2026-11-20T23:01:00+00:00", "gate3_consent_pending"),
            ("2026-11-21T09:00:00+00:00", "gate1_pass"),
        ] {
            put(&mut c, at, "r1", 2, ev, Some("p"), "[]", "{}", "[]", 0).await;
        }
        let a = disclosure_aggregates(&mut c, None, 0).await.unwrap();
        assert_eq!(a.actions.len(), 1);
        let x = &a.actions[0];
        assert_eq!(x.day, "2026-11-21");
        assert!(x.gate1 && x.marked && !x.approved && !x.held);
    }

    #[tokio::test]
    async fn since_bounds_the_window_and_a_declined_flag_splits_a_group() {
        let mut c = personal().await;
        put(
            &mut c,
            "2026-09-01T10:00:00+00:00",
            "old",
            2,
            "gate1_pass",
            Some("p"),
            "[]",
            "{}",
            "[]",
            0,
        )
        .await;
        put(
            &mut c,
            "2026-11-20T10:00:00+00:00",
            "r",
            2,
            "output_scan_full",
            None,
            "[]",
            "{}",
            "[]",
            1,
        )
        .await;
        put(
            &mut c,
            "2026-11-20T11:00:00+00:00",
            "r",
            2,
            "output_scan_full",
            None,
            "[]",
            "{}",
            "[]",
            0,
        )
        .await;
        let w = disclosure_aggregates(&mut c, Some("2026-10-23T00:00:00+00:00"), 0)
            .await
            .unwrap();
        assert!(w.events.iter().all(|a| a.focus_run_id != "old"));
        assert!(w.actions.is_empty(), "the old send is outside the window");
        assert_eq!(
            w.events.len(),
            2,
            "declined and not declined stay apart: {w:?}"
        );
    }

    #[tokio::test]
    async fn no_abstracted_value_ever_surfaces_only_the_names() {
        let mut c = personal().await;
        put(
            &mut c,
            "2026-11-20T10:00:00+00:00",
            "r1",
            2,
            "gate1_pass",
            Some("p1"),
            "[\"job_title\"]",
            &format!("{{\"employer\":\"{SENTINEL}\"}}"),
            "[]",
            0,
        )
        .await;
        // The same row as a background event, so its shared count is observable too.
        put(
            &mut c,
            "2026-11-20T10:01:00+00:00",
            "r1",
            2,
            "gate2_contamination_detected",
            Some("p1"),
            "[\"job_title\"]",
            &format!("{{\"employer\":\"{SENTINEL}\"}}"),
            "[]",
            0,
        )
        .await;
        let agg = disclosure_aggregates(&mut c, None, 0).await.unwrap();
        let det = disclosure_details(&mut c, &["r1".to_string()], 0)
            .await
            .unwrap();
        let all = format!("{agg:?} {det:?}");
        assert!(!all.contains(SENTINEL), "an abstracted value leaked: {all}");
        let send = det.iter().find(|d| d.event_type == "gate1_pass").unwrap();
        assert_eq!(send.shared, vec!["job_title", "employer"]);
        assert_eq!(
            (
                send.focus_run_id.as_str(),
                send.action_id.as_str(),
                send.day.as_str()
            ),
            ("r1", "a", "2026-11-20")
        );
        // The event row counts the abstracted field as shared.
        assert_eq!(agg.events[0].shared, 2);
    }

    #[tokio::test]
    async fn malformed_json_columns_count_as_zero_and_never_fail_the_read() {
        let mut c = personal().await;
        put(
            &mut c,
            "2026-11-20T10:00:00+00:00",
            "r1",
            2,
            "gate1_pass",
            Some("p"),
            "not json",
            "{{{",
            "also not",
            0,
        )
        .await;
        sqlx::query("UPDATE disclosure_log SET extra_metadata = 'garbage'")
            .execute(&mut c)
            .await
            .unwrap();
        let agg = disclosure_aggregates(&mut c, None, 0).await.unwrap();
        let e = &agg.events[0];
        assert_eq!((e.shared, e.withheld, e.event_type.as_str()), (0, 0, ""));
        assert!(agg.actions.is_empty(), "a row with no event is no send");
        let det = disclosure_details(&mut c, &["r1".to_string()], 0)
            .await
            .unwrap();
        assert!(det[0].shared.is_empty() && det[0].withheld.is_empty());
    }

    #[tokio::test]
    async fn consent_rows_keep_an_unknown_type_as_a_string() {
        let mut c = personal().await;
        for (id, ty, ref_id) in [
            ("a", "collection_consent_granted", None),
            ("b", "some_future_type", Some("a")),
        ] {
            sqlx::query(
                "INSERT INTO consent_audit_records
                 (id, event_type, focus_slug, consent_reference_id, content_summary, created_at)
                 VALUES (?, ?, 'tech-support', ?, 'A fixed description.', '2026-11-20T10:00:00+00:00')",
            )
            .bind(id).bind(ty).bind(ref_id)
            .execute(&mut c).await.unwrap();
        }
        let rows = consent_rows(&mut c, None, 0).await.unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1].event_type, "some_future_type");
        assert!(rows[1].has_reference && !rows[0].has_reference);
    }

    #[tokio::test]
    async fn run_focus_map_joins_in_chunks_and_omits_a_missing_run() {
        let mut o = mem(&[include_str!("../../schema/outputs_001.sql")]).await;
        let ids: Vec<String> = (0..1200).map(|i| format!("run-{i}")).collect();
        for id in ids.iter().take(1100) {
            sqlx::query("INSERT INTO focus_runs (id, focus_id, started_at) VALUES (?, 'writing-assistant', 't')")
                .bind(id).execute(&mut o).await.unwrap();
        }
        let map = run_focus_map(&mut o, &ids).await.unwrap();
        assert_eq!(map.len(), 1100);
        assert!(!map.contains_key("run-1150"));
        assert!(run_focus_map(&mut o, &[]).await.unwrap().is_empty());
    }

    /// Not a gate: measures the window scan at 10^5 rows (items.id=957 ruling 5: add
    /// a created_at index if this is over 100 ms) and the size of the summary payload
    /// built from it. The rows are shaped like real sends (items.id=958 unit 2): half
    /// of them a gate1_pass, half the review's row for the same action, a few asked,
    /// held or approved, three actions to a run. Run with
    /// `cargo test --lib scan_timing -- --ignored --nocapture`.
    #[tokio::test]
    #[ignore]
    async fn scan_timing_at_100k_rows() {
        use crate::commands::shared_log::{build_summary, Filters, Lookups, SharedLogWindow};
        let mut c = personal().await;
        sqlx::query("BEGIN").execute(&mut c).await.unwrap();
        let evs = [
            "gate1_pass",
            "gate3_pf_no_spans",
            "gate3_promotion_approved",
            "gate3_consent_pending",
            "gate3_pf_unavailable_block",
        ];
        for i in 0..100_000i64 {
            let k = i / 2; // the action
            let ev = if i % 2 == 0 {
                evs[0]
            } else {
                match k % 20 {
                    0 => evs[2],
                    1 => evs[3],
                    2 => evs[4],
                    _ => evs[1],
                }
            };
            let day = 1 + (i % 360) / 12; // ~30 days of the newest month, older spread below
            let (month, d) = if k % 4 == 0 {
                (11, 1 + (k / 4) % 22)
            } else {
                (1 + (k % 10), day.min(28))
            };
            sqlx::query(
                "INSERT INTO disclosure_log
                 (id, user_id, persona_id, focus_run_id, action_id, routing_tier, provider,
                  fields_shared, fields_abstracted, fields_withheld, override_declined,
                  created_at, extra_metadata, execution_tier)
                 VALUES (?, 'u', 'p', ?, ?, 2, 'prov', '[\"job_title\",\"employer\"]',
                         '{\"industry\":\"x\"}', '[\"location_city\"]', 0, ?, ?, 2)",
            )
            .bind(i.to_string())
            .bind(format!("run-{}", k / 3))
            .bind(format!("act-{k}"))
            .bind(format!("2026-{month:02}-{d:02}T10:00:00+00:00"))
            .bind(format!("{{\"event_type\":\"{ev}\"}}"))
            .execute(&mut c)
            .await
            .unwrap();
        }
        sqlx::query("COMMIT").execute(&mut c).await.unwrap();
        for since in [
            Some("2026-10-23T00:00:00+00:00"),
            Some("2026-11-16T00:00:00+00:00"),
            None,
        ] {
            let mut best = std::time::Duration::MAX;
            let mut agg = DisclosureAggregates::default();
            for _ in 0..3 {
                let t = std::time::Instant::now();
                agg = disclosure_aggregates(&mut c, since, 0).await.unwrap();
                best = best.min(t.elapsed());
            }
            let run_focus: HashMap<String, String> = agg
                .actions
                .iter()
                .map(|a| a.focus_run_id.clone())
                .chain(agg.events.iter().map(|a| a.focus_run_id.clone()))
                .map(|r| (r, "writing-assistant".to_owned()))
                .collect();
            let (events, actions) = (agg.events.len(), agg.actions.len());
            let raw = SharedLogRaw {
                aggregates: agg.events,
                actions: agg.actions,
                consent: vec![],
                run_focus,
            };
            let t = std::time::Instant::now();
            let summary = build_summary(
                &raw,
                &Lookups {
                    focus_names: HashMap::new(),
                    providers: HashMap::new(),
                },
                SharedLogWindow::Last30,
                chrono::NaiveDate::from_ymd_opt(2026, 11, 22).unwrap(),
                &Filters {
                    focus_id: None,
                    provider_id: None,
                },
            );
            println!(
                "since={since:?}: {events} event rows + {actions} action rows in {best:?} (best \
                 of 3); build_summary {:?}; {} groups, {} months, payload {} bytes",
                t.elapsed(),
                summary.groups.len(),
                summary.months.len(),
                serde_json::to_vec(&summary).unwrap().len()
            );
        }
    }

    #[test]
    fn the_offset_modifier_is_clamped_and_signed() {
        assert_eq!(offset_modifier(60), "+60 minutes");
        assert_eq!(offset_modifier(-480), "-480 minutes");
        assert_eq!(offset_modifier(99_999), "+840 minutes");
        assert_eq!(offset_modifier(-99_999), "-840 minutes");
    }

    /// The module reads and never writes, and the abstracted column is touched only
    /// through json_each (its values are never selected).
    #[test]
    fn this_module_only_reads_and_never_selects_abstracted_values() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src/persistence/shared_log_read.rs");
        let text = std::fs::read_to_string(path).expect("source readable");
        let production = text.split("#[cfg(test)]").next().unwrap().to_lowercase();
        let code: Vec<&str> = production
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect();
        for forbidden in [
            "insert into",
            "update ",
            "delete from",
            "replace into",
            "drop table",
            "alter table",
        ] {
            let hits: Vec<_> = code
                .iter()
                .filter(|l| l.contains(forbidden) && !l.contains("on update"))
                .collect();
            assert!(hits.is_empty(), "`{forbidden}`: {hits:?}");
        }
        for (i, line) in code.iter().enumerate() {
            if line.contains("fields_abstracted") {
                // Every mention is a json_valid guard or a json_each key read.
                let window = code[i.saturating_sub(1)..(i + 2).min(code.len())].join(" ");
                assert!(
                    window.contains("json_each") || window.contains("json_valid"),
                    "bare fields_abstracted: {line}"
                );
            }
        }
    }
}
