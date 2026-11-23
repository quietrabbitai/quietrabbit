// src-tauri/src/persistence/consent_history_purge.rs
//
// items.id=957 unit 3 (decisions.id=434, 944, 967; Chat-PM ruling on Jason's call):
// the user-initiated CLEAN-UP of a Persona's older consent records. GATE-CRITICAL
// (destructive, failure direction).
//
// IT IS HOUSEKEEPING, NOT A PURGE OF THE CONSENTS THEMSELVES. It never deletes a
// record that shows a consent currently in force.
//
// WHAT IS KEPT, never deleted (the one rule, `deletable_ids`):
//   (a) every history_cleared marker;
//   (b) per Focus, the newest collection_consent_* record (granted or withdrawn),
//       IF its type is collection_consent_granted: that grant is in force;
//   (c) per Focus and document type, the newest document_ceiling_raised record:
//       that raise is in force.
// EVERYTHING ELSE is deleted: superseded grants, withdrawals (a withdrawn Focus
// keeps no collection record), older raises, and every other event type.
// "Newest" is the store's own order, created_at then rowid (consent_audit_store::list,
// the same list latest_collection_event in commands/collection_consent.rs and
// previous_raise in commands/document_raise.rs read, with the same rev/rfind), so a
// kept record is exactly the one those two see after the purge.
//
// WHAT HAPPENS, in ONE transaction on the Persona's personal.db:
//   1. read every Focus's records through the store's list, work out the ids to delete,
//   2. delete those rows (one DELETE by id each),
//   3. append one new history_cleared marker (the store's validated append).
// Either all of it happens or none: if the marker cannot be written the deletes are
// rolled back and every record is still there (failure direction: nothing is lost
// without the permanent "cleared on" line). A purge never removes a marker, so the
// line is permanent and repeats keep every earlier one.
// With nothing to delete it writes nothing and returns NothingToClear (a purge that
// only kept records leaves no second marker).
//
// WHY A SEPARATE MODULE: consent_audit_store.rs is append-only by design, pinned by a
// source-scan test, and its own comment says a purge is "a separate user-initiated
// function, not an addition to that module". It gained one enum variant and nothing
// else. disclosure_log is never deleted (personal_001.sql, D6-198): this module does
// not name that table, and a source scan pins it.
//
// A record whose event_type this version does not know makes the store's list fail,
// and the purge then fails closed (nothing is deleted).

use std::collections::HashSet;

use sqlx::SqliteConnection;
use thiserror::Error;

use crate::persistence::consent_audit_store::{
    self as store, ConsentAuditError, ConsentAuditRecord, ConsentEventType, NewConsentEvent,
};

/// The marker's focus_slug (the column is NOT NULL and the marker belongs to no Focus).
pub const MARKER_FOCUS_SLUG: &str = "*";
/// The marker's fixed description. Never content.
pub const MARKER_SUMMARY: &str = "The user cleared the consent history.";

#[derive(Debug, Error)]
pub enum PurgeError {
    #[error("nothing_to_clear")]
    NothingToClear,
    #[error("Database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("{0}")]
    Marker(#[from] ConsentAuditError),
    #[error("purge interrupted: {0}")]
    Interrupted(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PurgeOutcome {
    /// Consent records removed (the markers are not counted and not removed).
    pub removed: u64,
    /// The new marker's id.
    pub marker_id: String,
}

/// One record as the keep rule reads it: its id, its type string and its document type.
#[derive(Debug, Clone, Copy)]
pub struct KeepRow<'a> {
    pub id: &'a str,
    pub event_type: &'a str,
    pub document_type: Option<&'a str>,
}

/// THE DELETE RULE. `rows` are the records of ONE Focus, oldest first (created_at
/// then rowid). Returns the ids to delete: everything except a history_cleared
/// marker, the newest collection_consent_* record when it is a grant, and the newest
/// document_ceiling_raised record of each document type.
pub fn deletable_ids<'a>(rows: &[KeepRow<'a>]) -> Vec<&'a str> {
    let granted = ConsentEventType::CollectionConsentGranted.as_str();
    let withdrawn = ConsentEventType::CollectionConsentWithdrawn.as_str();
    let raised = ConsentEventType::DocumentCeilingRaised.as_str();
    let marker = ConsentEventType::HistoryCleared.as_str();
    let mut keep: HashSet<&str> = HashSet::new();
    // Same idiom as latest_collection_event: the newest granted-or-withdrawn.
    if let Some(r) = rows
        .iter()
        .rev()
        .find(|r| r.event_type == granted || r.event_type == withdrawn)
    {
        if r.event_type == granted {
            keep.insert(r.id);
        }
    }
    // Same idiom as previous_raise: the newest raise, per document type.
    let types: HashSet<Option<&str>> = rows
        .iter()
        .filter(|r| r.event_type == raised)
        .map(|r| r.document_type)
        .collect();
    for ty in types {
        if let Some(r) = rows
            .iter()
            .rfind(|r| r.event_type == raised && r.document_type == ty)
        {
            keep.insert(r.id);
        }
    }
    rows.iter()
        .filter(|r| r.event_type != marker && !keep.contains(r.id))
        .map(|r| r.id)
        .collect()
}

fn keep_row(r: &ConsentAuditRecord) -> KeepRow<'_> {
    KeepRow {
        id: &r.id,
        event_type: r.event_type.as_str(),
        document_type: r.document_type.as_deref(),
    }
}

/// Purge the older consent records of the Persona whose personal.db `conn` is open.
pub async fn purge_consent_history(
    conn: &mut SqliteConnection,
) -> Result<PurgeOutcome, PurgeError> {
    purge_with_hook(conn, || Ok(())).await
}

/// `purge_consent_history` with a hook that runs between the deletes and the marker
/// write, so a test can make the second half fail and prove the first is undone.
pub(crate) async fn purge_with_hook<F>(
    conn: &mut SqliteConnection,
    between: F,
) -> Result<PurgeOutcome, PurgeError>
where
    F: FnOnce() -> Result<(), String>,
{
    let mut tx = sqlx::Connection::begin(&mut *conn).await?;
    // Every Focus that has a record, read through the store's own ordered list.
    let slugs: Vec<String> = sqlx::query_scalar(
        "SELECT DISTINCT focus_slug FROM consent_audit_records ORDER BY focus_slug",
    )
    .fetch_all(&mut *tx)
    .await?;
    let mut doomed: Vec<String> = Vec::new();
    for slug in &slugs {
        let records = store::list(&mut tx, slug).await?;
        let rows: Vec<KeepRow<'_>> = records.iter().map(keep_row).collect();
        doomed.extend(deletable_ids(&rows).into_iter().map(str::to_owned));
    }
    if doomed.is_empty() {
        // Nothing to remove, so nothing to record: roll back (a no-op).
        tx.rollback().await?;
        return Err(PurgeError::NothingToClear);
    }
    let mut removed = 0u64;
    for id in &doomed {
        removed += sqlx::query("DELETE FROM consent_audit_records WHERE id = ?")
            .bind(id)
            .execute(&mut *tx)
            .await?
            .rows_affected();
    }
    between().map_err(PurgeError::Interrupted)?; // drops tx: rolled back
    let marker_id = store::append(
        &mut tx,
        &NewConsentEvent {
            event_type: ConsentEventType::HistoryCleared,
            focus_slug: MARKER_FOCUS_SLUG,
            run_id: None,
            text_version: None,
            consent_reference_id: None,
            content_summary: MARKER_SUMMARY,
            document_type: None,
        },
    )
    .await?; // an Err drops tx: rolled back, every record is still there
    tx.commit().await?;
    Ok(PurgeOutcome { removed, marker_id })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persistence::migrations::parse_statements;
    use sqlx::sqlite::SqliteConnectOptions;
    use sqlx::{ConnectOptions, Row};

    async fn db() -> SqliteConnection {
        let mut conn = SqliteConnectOptions::new()
            .filename(":memory:")
            .connect()
            .await
            .unwrap();
        for sql in [
            include_str!("../../schema/personal_001.sql"),
            include_str!("../../schema/personal_008.sql"),
            include_str!("../../schema/personal_010.sql"),
        ] {
            for stmt in parse_statements(sql) {
                sqlx::query(&stmt).execute(&mut conn).await.unwrap();
            }
        }
        conn
    }

    /// Append one record and return its id.
    async fn put(
        conn: &mut SqliteConnection,
        t: ConsentEventType,
        focus: &str,
        doc: Option<&str>,
    ) -> String {
        store::append(
            conn,
            &NewConsentEvent {
                event_type: t,
                focus_slug: focus,
                run_id: None,
                text_version: Some(1),
                consent_reference_id: None,
                content_summary: "A fixed description.",
                document_type: doc,
            },
        )
        .await
        .unwrap()
    }

    async fn seed_disclosure(conn: &mut SqliteConnection, n: i64) {
        for i in 0..n {
            sqlx::query(
                "INSERT INTO disclosure_log
                 (id, user_id, persona_id, focus_run_id, action_id, routing_tier, created_at,
                  extra_metadata, execution_tier)
                 VALUES (?, 'u', 'p', 'r', 'a', 2, '2026-11-20T10:00:00+00:00',
                         '{\"event_type\":\"gate1_pass\"}', 2)",
            )
            .bind(i.to_string())
            .execute(&mut *conn)
            .await
            .unwrap();
        }
    }

    async fn ids(conn: &mut SqliteConnection) -> Vec<String> {
        sqlx::query("SELECT id FROM consent_audit_records ORDER BY created_at, rowid")
            .fetch_all(conn)
            .await
            .unwrap()
            .iter()
            .map(|r| r.get("id"))
            .collect()
    }

    async fn count(conn: &mut SqliteConnection, sql: &str) -> i64 {
        sqlx::query(sql)
            .fetch_one(conn)
            .await
            .unwrap()
            .get::<i64, _>(0)
    }

    use ConsentEventType::*;

    fn row<'a>(id: &'a str, t: ConsentEventType, doc: Option<&'a str>) -> KeepRow<'a> {
        KeepRow {
            id,
            event_type: t.as_str(),
            document_type: doc,
        }
    }

    /// THE RULE, as a table. Rows are one Focus, oldest first.
    #[test]
    fn the_delete_rule_keeps_exactly_what_is_in_force() {
        // A newest grant stays; the older grant and the withdrawal go.
        let rows = [
            row("g1", CollectionConsentGranted, None),
            row("w1", CollectionConsentWithdrawn, None),
            row("g2", CollectionConsentGranted, None),
        ];
        assert_eq!(deletable_ids(&rows), vec!["g1", "w1"]);
        // A newest withdrawal: no collection record is kept.
        let rows = [
            row("g1", CollectionConsentGranted, None),
            row("w1", CollectionConsentWithdrawn, None),
        ];
        assert_eq!(deletable_ids(&rows), vec!["g1", "w1"]);
        // The newest raise of EACH document type stays, older raises and other types go.
        let rows = [
            row("r1", DocumentCeilingRaised, Some("cover_letter")),
            row("r2", DocumentCeilingRaised, Some("cover_letter")),
            row("r3", DocumentCeilingRaised, Some("essay_section")),
            row("s1", SensitivityOverride, None),
            row("t1", Tier3Generation, None),
        ];
        assert_eq!(deletable_ids(&rows), vec!["r1", "s1", "t1"]);
        // A marker is never deletable, wherever it sits.
        let rows = [
            row("m1", HistoryCleared, None),
            row("s1", StandingConsentUse, None),
        ];
        assert_eq!(deletable_ids(&rows), vec!["s1"]);
        // Nothing but in-force records: nothing to delete.
        let rows = [
            row("g1", CollectionConsentGranted, None),
            row("r1", DocumentCeilingRaised, Some("cover_letter")),
            row("m1", HistoryCleared, None),
        ];
        assert!(deletable_ids(&rows).is_empty());
        assert!(deletable_ids(&[]).is_empty());
    }

    #[tokio::test]
    async fn in_force_records_survive_and_the_rest_are_removed_with_one_marker() {
        let mut c = db().await;
        let g_old = put(&mut c, CollectionConsentGranted, "tech-support", None).await;
        let w = put(&mut c, CollectionConsentWithdrawn, "tech-support", None).await;
        let g_new = put(&mut c, CollectionConsentGranted, "tech-support", None).await;
        let r_old = put(
            &mut c,
            DocumentCeilingRaised,
            "writing-assistant",
            Some("cover_letter"),
        )
        .await;
        let r_new = put(
            &mut c,
            DocumentCeilingRaised,
            "writing-assistant",
            Some("cover_letter"),
        )
        .await;
        let other = put(&mut c, SensitivityOverride, "writing-assistant", None).await;
        // A Focus whose newest collection record is a withdrawal keeps none.
        let g_x = put(&mut c, CollectionConsentGranted, "other-focus", None).await;
        let w_x = put(&mut c, CollectionConsentWithdrawn, "other-focus", None).await;
        seed_disclosure(&mut c, 5).await;

        let out = purge_consent_history(&mut c).await.unwrap();
        assert_eq!(out.removed, 6);
        let left = ids(&mut c).await;
        assert!(
            left.contains(&g_new) && left.contains(&r_new),
            "in force survives"
        );
        for gone in [&g_old, &w, &r_old, &other, &g_x, &w_x] {
            assert!(!left.contains(gone), "{gone} should be deleted");
        }
        assert!(left.contains(&out.marker_id));
        assert_eq!(left.len(), 3, "two in-force records and one marker");
        assert_eq!(
            count(&mut c, "SELECT COUNT(*) FROM disclosure_log").await,
            5
        );
    }

    #[tokio::test]
    async fn a_second_purge_with_only_kept_records_writes_no_second_marker() {
        let mut c = db().await;
        put(&mut c, CollectionConsentGranted, "tech-support", None).await;
        put(&mut c, SensitivityOverride, "tech-support", None).await;
        let first = purge_consent_history(&mut c).await.unwrap();
        assert_eq!(first.removed, 1);
        let before = ids(&mut c).await;
        assert!(matches!(
            purge_consent_history(&mut c).await,
            Err(PurgeError::NothingToClear)
        ));
        assert_eq!(ids(&mut c).await, before);
        // A later purge keeps the earlier marker and adds its own.
        put(&mut c, StandingConsentUse, "tech-support", None).await;
        let second = purge_consent_history(&mut c).await.unwrap();
        let left = ids(&mut c).await;
        assert!(left.contains(&first.marker_id) && left.contains(&second.marker_id));
    }

    #[tokio::test]
    async fn nothing_to_clear_writes_nothing() {
        let mut c = db().await;
        assert!(matches!(
            purge_consent_history(&mut c).await,
            Err(PurgeError::NothingToClear)
        ));
        put(
            &mut c,
            DocumentCeilingRaised,
            "writing-assistant",
            Some("cover_letter"),
        )
        .await;
        assert!(matches!(
            purge_consent_history(&mut c).await,
            Err(PurgeError::NothingToClear)
        ));
        assert_eq!(
            count(&mut c, "SELECT COUNT(*) FROM consent_audit_records").await,
            1
        );
    }

    /// FAILURE DIRECTION: if the second half fails, every delete is undone.
    #[tokio::test]
    async fn a_failure_after_the_deletes_rolls_them_back_and_loses_nothing() {
        let mut c = db().await;
        put(&mut c, CollectionConsentGranted, "tech-support", None).await;
        put(&mut c, SensitivityOverride, "writing-assistant", None).await;
        put(&mut c, StandingConsentUse, "writing-assistant", None).await;
        let before = ids(&mut c).await;
        let err = purge_with_hook(&mut c, || Err("injected".to_owned())).await;
        assert!(matches!(err, Err(PurgeError::Interrupted(_))));
        assert_eq!(ids(&mut c).await, before);
    }

    /// The marker write itself failing also rolls the deletes back.
    #[tokio::test]
    async fn a_marker_that_cannot_be_written_rolls_the_deletes_back() {
        let mut c = db().await;
        put(&mut c, CollectionConsentGranted, "tech-support", None).await;
        put(&mut c, SensitivityOverride, "tech-support", None).await;
        sqlx::query(
            "CREATE TRIGGER refuse_marker BEFORE INSERT ON consent_audit_records
             WHEN NEW.event_type = 'history_cleared'
             BEGIN SELECT RAISE(ABORT, 'refused'); END",
        )
        .execute(&mut c)
        .await
        .unwrap();
        let before = ids(&mut c).await;
        assert!(purge_consent_history(&mut c).await.is_err());
        assert_eq!(ids(&mut c).await, before);
    }

    /// The module never names disclosure_log, its only DELETE is by id, and no path
    /// reaches a marker.
    #[test]
    fn this_module_never_touches_disclosure_log_and_never_deletes_a_marker() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src/persistence/consent_history_purge.rs");
        let text = std::fs::read_to_string(path).expect("source readable");
        let production = text.split("#[cfg(test)]").next().unwrap().to_lowercase();
        let code: Vec<&str> = production
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect();
        assert!(
            !code.iter().any(|l| l.contains("disclosure_log")),
            "the purge names disclosure_log"
        );
        let deletes: Vec<&&str> = code.iter().filter(|l| l.contains("delete from")).collect();
        assert_eq!(deletes.len(), 1, "{deletes:?}");
        assert!(deletes[0].contains("delete from consent_audit_records where id = ?"));
        // The rule skips a marker by type.
        assert!(production.contains("r.event_type != marker"));
        for forbidden in [
            "drop table",
            "truncate",
            "alter table",
            "replace into",
            "update consent",
        ] {
            assert!(!code.iter().any(|l| l.contains(forbidden)), "{forbidden}");
        }
    }
}
