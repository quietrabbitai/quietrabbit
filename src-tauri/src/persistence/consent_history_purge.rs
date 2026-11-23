// src-tauri/src/persistence/consent_history_purge.rs
//
// items.id=957 unit 3 (decisions.id=434, 944, 967): the user-initiated purge of a
// Persona's consent records. GATE-CRITICAL (destructive, failure direction).
//
// WHY A SEPARATE MODULE: persistence/consent_audit_store.rs is append-only by
// design, pinned by a source-scan test, and its own comment says a purge is "a
// separate user-initiated function, not an addition to that module". This is that
// function. consent_audit_store.rs gains one enum variant (HistoryCleared) and
// nothing else.
//
// WHAT IT DOES, in ONE transaction on the Persona's personal.db:
//   1. DELETE every consent_audit_records row EXCEPT history_cleared markers,
//   2. append one new history_cleared marker (through the store's validated append).
// Either both happen or neither: if the marker cannot be written the delete is
// rolled back and every record is still there (failure direction: nothing is lost
// without the permanent "cleared on" line). A purge never removes a history_cleared
// marker, so the line is permanent and repeated purges keep every earlier one.
//
// WHAT IT NEVER TOUCHES: disclosure_log is never deleted (personal_001.sql, D6-198).
// This module does not name that table, and a source scan pins it.
//
// With nothing to remove it does nothing and returns NothingToClear: no marker is
// written for a purge that cleared nothing.

use sqlx::SqliteConnection;
use thiserror::Error;

use crate::persistence::consent_audit_store::{
    self as store, ConsentAuditError, ConsentEventType, NewConsentEvent,
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

/// Purge the consent records of the Persona whose personal.db `conn` is open.
pub async fn purge_consent_history(
    conn: &mut SqliteConnection,
) -> Result<PurgeOutcome, PurgeError> {
    purge_with_hook(conn, || Ok(())).await
}

/// `purge_consent_history` with a hook that runs between the delete and the marker
/// write, so a test can make the second half fail and prove the first is undone.
pub(crate) async fn purge_with_hook<F>(
    conn: &mut SqliteConnection,
    between: F,
) -> Result<PurgeOutcome, PurgeError>
where
    F: FnOnce() -> Result<(), String>,
{
    let mut tx = sqlx::Connection::begin(&mut *conn).await?;
    let removed = sqlx::query("DELETE FROM consent_audit_records WHERE event_type != ?")
        .bind(ConsentEventType::HistoryCleared.as_str())
        .execute(&mut *tx)
        .await?
        .rows_affected();
    if removed == 0 {
        // Nothing was removed, so there is nothing to record: roll back (a no-op).
        tx.rollback().await?;
        return Err(PurgeError::NothingToClear);
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

    async fn seed_consent(conn: &mut SqliteConnection, t: ConsentEventType, focus: &str) {
        store::append(
            conn,
            &NewConsentEvent {
                event_type: t,
                focus_slug: focus,
                run_id: None,
                text_version: Some(1),
                consent_reference_id: None,
                content_summary: "A fixed description.",
                document_type: (t == ConsentEventType::DocumentCeilingRaised)
                    .then_some("cover_letter"),
            },
        )
        .await
        .unwrap();
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

    async fn count(conn: &mut SqliteConnection, sql: &str) -> i64 {
        sqlx::query(sql)
            .fetch_one(conn)
            .await
            .unwrap()
            .get::<i64, _>(0)
    }

    #[tokio::test]
    async fn a_purge_removes_consent_records_only_and_leaves_one_marker() {
        let mut c = db().await;
        seed_consent(
            &mut c,
            ConsentEventType::CollectionConsentGranted,
            "tech-support",
        )
        .await;
        seed_consent(
            &mut c,
            ConsentEventType::DocumentCeilingRaised,
            "writing-assistant",
        )
        .await;
        seed_consent(
            &mut c,
            ConsentEventType::SensitivityOverride,
            "writing-assistant",
        )
        .await;
        seed_disclosure(&mut c, 5).await;

        let out = purge_consent_history(&mut c).await.unwrap();
        assert_eq!(out.removed, 3);
        // Only the marker is left, and disclosure_log is untouched.
        let rows = sqlx::query("SELECT id, event_type, focus_slug FROM consent_audit_records")
            .fetch_all(&mut c)
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get::<String, _>("event_type"), "history_cleared");
        assert_eq!(rows[0].get::<String, _>("id"), out.marker_id);
        assert_eq!(
            count(&mut c, "SELECT COUNT(*) FROM disclosure_log").await,
            5
        );
    }

    #[tokio::test]
    async fn a_later_purge_never_removes_an_earlier_marker() {
        let mut c = db().await;
        seed_consent(
            &mut c,
            ConsentEventType::CollectionConsentGranted,
            "tech-support",
        )
        .await;
        let first = purge_consent_history(&mut c).await.unwrap();
        // A purge with nothing to clear writes nothing and removes nothing.
        assert!(matches!(
            purge_consent_history(&mut c).await,
            Err(PurgeError::NothingToClear)
        ));
        assert_eq!(
            count(&mut c, "SELECT COUNT(*) FROM consent_audit_records").await,
            1
        );
        seed_consent(
            &mut c,
            ConsentEventType::CollectionConsentWithdrawn,
            "tech-support",
        )
        .await;
        let second = purge_consent_history(&mut c).await.unwrap();
        assert_eq!(second.removed, 1);
        let ids: Vec<String> = sqlx::query("SELECT id FROM consent_audit_records ORDER BY rowid")
            .fetch_all(&mut c)
            .await
            .unwrap()
            .iter()
            .map(|r| r.get("id"))
            .collect();
        assert_eq!(ids, vec![first.marker_id, second.marker_id]);
    }

    /// FAILURE DIRECTION: if the second half fails, the delete is undone.
    #[tokio::test]
    async fn a_failure_after_the_delete_rolls_it_back_and_loses_nothing() {
        let mut c = db().await;
        seed_consent(
            &mut c,
            ConsentEventType::CollectionConsentGranted,
            "tech-support",
        )
        .await;
        seed_consent(
            &mut c,
            ConsentEventType::SensitivityOverride,
            "writing-assistant",
        )
        .await;
        let err = purge_with_hook(&mut c, || Err("injected".to_owned())).await;
        assert!(matches!(err, Err(PurgeError::Interrupted(_))));
        assert_eq!(
            count(&mut c, "SELECT COUNT(*) FROM consent_audit_records").await,
            2
        );
        assert_eq!(
            count(
                &mut c,
                "SELECT COUNT(*) FROM consent_audit_records WHERE event_type = 'history_cleared'"
            )
            .await,
            0
        );
    }

    /// The marker write itself failing (here: the table refuses the row) also
    /// rolls the delete back.
    #[tokio::test]
    async fn a_marker_that_cannot_be_written_rolls_the_delete_back() {
        let mut c = db().await;
        seed_consent(
            &mut c,
            ConsentEventType::CollectionConsentGranted,
            "tech-support",
        )
        .await;
        // A trigger that refuses a marker insert.
        sqlx::query(
            "CREATE TRIGGER refuse_marker BEFORE INSERT ON consent_audit_records
             WHEN NEW.event_type = 'history_cleared'
             BEGIN SELECT RAISE(ABORT, 'refused'); END",
        )
        .execute(&mut c)
        .await
        .unwrap();
        assert!(purge_consent_history(&mut c).await.is_err());
        assert_eq!(
            count(&mut c, "SELECT COUNT(*) FROM consent_audit_records").await,
            1
        );
        assert_eq!(
            count(&mut c, "SELECT COUNT(*) FROM consent_audit_records WHERE event_type = 'collection_consent_granted'").await,
            1
        );
    }

    /// The module deletes consent records only, never names disclosure_log, and its
    /// one DELETE keeps history_cleared.
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
        assert!(deletes[0].contains("consent_audit_records where event_type != ?"));
        // The bound value is the marker's own type.
        assert!(production.contains(".bind(consenteventtype::historycleared.as_str())"));
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
