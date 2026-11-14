// src-tauri/src/persistence/consent_audit_store.rs
//
// items.id=920 stage 1 (decisions.id=944): the permanent audit trail of consent
// events, backed by consent_audit_records in the Persona's encrypted personal.db
// (schema/personal_010.sql), next to disclosure_log.
//
// APPEND-ONLY, PERMANENT, ON PURPOSE, like disclosure_log_store.rs: this module
// exposes append, list and latest_for_focus and NOTHING else. There is no UPDATE
// and no DELETE, and a source-scan test below fails if one is ever added. A purge
// is a separate, user-initiated function and is not part of this item.
//
// WHO WRITES. A model must never be able to record consent. append() has no
// caller outside tests yet: stage 2's command (the user's own confirm) is its
// only intended caller, together with focus_settings_store::set_collection_consent.
// A source scan over conductor/store_writer.rs, store_op.rs and lifecycle.rs
// (conductor/store_op_tests.rs) pins that nothing in the conductor names
// the table.
//
// WHAT A ROW HOLDS. content_summary is a FIXED abstract description of the event
// chosen by the caller from its own constants, never user or model content.
// There is no persona_id column: the file boundary implies it (personal_009.sql,
// persona_floor_consent).
//
// CONNECTION MODEL: functions take the caller's &mut SqliteConnection to an
// opened personal.db (open_personal_db), like every other per-Persona store here.

use sqlx::{Row, SqliteConnection};
use thiserror::Error;

/// The kinds of consent event. The column has no CHECK (SQLite cannot alter a
/// CHECK without a table rebuild), so this enum is the validation, and
/// `ALL` plus the round-trip test pin it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsentEventType {
    /// A grant of silent collection for a Focus. A re-consent is the same type,
    /// pointing at the prior event with a new text_version.
    CollectionConsentGranted,
    CollectionConsentWithdrawn,
    /// The Writing Assistant raising a document-type ceiling (carries
    /// document_type).
    DocumentCeilingRaised,
    // The five types of decisions.id=434.
    Tier3Generation,
    Tier3Validation,
    SensitivityOverride,
    StandingConsentUse,
    OutputPrivacyDisclosure,
}

impl ConsentEventType {
    pub const ALL: [ConsentEventType; 8] = [
        ConsentEventType::CollectionConsentGranted,
        ConsentEventType::CollectionConsentWithdrawn,
        ConsentEventType::DocumentCeilingRaised,
        ConsentEventType::Tier3Generation,
        ConsentEventType::Tier3Validation,
        ConsentEventType::SensitivityOverride,
        ConsentEventType::StandingConsentUse,
        ConsentEventType::OutputPrivacyDisclosure,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            ConsentEventType::CollectionConsentGranted => "collection_consent_granted",
            ConsentEventType::CollectionConsentWithdrawn => "collection_consent_withdrawn",
            ConsentEventType::DocumentCeilingRaised => "document_ceiling_raised",
            ConsentEventType::Tier3Generation => "tier3_generation",
            ConsentEventType::Tier3Validation => "tier3_validation",
            ConsentEventType::SensitivityOverride => "sensitivity_override",
            ConsentEventType::StandingConsentUse => "standing_consent_use",
            ConsentEventType::OutputPrivacyDisclosure => "output_privacy_disclosure",
        }
    }

    pub fn parse(s: &str) -> Option<ConsentEventType> {
        Self::ALL.into_iter().find(|t| t.as_str() == s)
    }
}

#[derive(Debug, Error)]
pub enum ConsentAuditError {
    #[error("Database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("Validation error: {0}")]
    Validation(String),
    #[error("consent_audit_records holds an unknown event_type '{0}'")]
    UnknownEventType(String),
}

/// One event to append.
#[derive(Debug, Clone)]
pub struct NewConsentEvent<'a> {
    pub event_type: ConsentEventType,
    pub focus_slug: &'a str,
    /// None for a setup event.
    pub run_id: Option<&'a str>,
    /// The version of the consent text that was shown.
    pub text_version: Option<i64>,
    /// The prior event in the chain.
    pub consent_reference_id: Option<&'a str>,
    /// A fixed abstract description, never user content.
    pub content_summary: &'a str,
    /// Required for document_ceiling_raised, forbidden otherwise.
    pub document_type: Option<&'a str>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsentAuditRecord {
    pub id: String,
    pub event_type: ConsentEventType,
    pub focus_slug: String,
    pub run_id: Option<String>,
    pub text_version: Option<i64>,
    pub consent_reference_id: Option<String>,
    pub content_summary: String,
    pub document_type: Option<String>,
    pub created_at: String,
}

fn row_to_record(row: &sqlx::sqlite::SqliteRow) -> Result<ConsentAuditRecord, ConsentAuditError> {
    let event_raw: String = row.try_get("event_type")?;
    let event_type = ConsentEventType::parse(&event_raw)
        .ok_or_else(|| ConsentAuditError::UnknownEventType(event_raw))?;
    Ok(ConsentAuditRecord {
        id: row.try_get("id")?,
        event_type,
        focus_slug: row.try_get("focus_slug")?,
        run_id: row.try_get("run_id")?,
        text_version: row.try_get("text_version")?,
        consent_reference_id: row.try_get("consent_reference_id")?,
        content_summary: row.try_get("content_summary")?,
        document_type: row.try_get("document_type")?,
        created_at: row.try_get("created_at")?,
    })
}

const COLUMNS: &str = "id, event_type, focus_slug, run_id, text_version, \
                       consent_reference_id, content_summary, document_type, created_at";

/// Append one event. Returns the new id (UUID v4).
pub async fn append(
    conn: &mut SqliteConnection,
    event: &NewConsentEvent<'_>,
) -> Result<String, ConsentAuditError> {
    if event.focus_slug.trim().is_empty() {
        return Err(ConsentAuditError::Validation(
            "focus_slug must not be empty".to_owned(),
        ));
    }
    if event.content_summary.trim().is_empty() {
        return Err(ConsentAuditError::Validation(
            "content_summary must not be empty".to_owned(),
        ));
    }
    let is_ceiling = event.event_type == ConsentEventType::DocumentCeilingRaised;
    match (is_ceiling, event.document_type.map(str::trim)) {
        (true, None) | (true, Some("")) => {
            return Err(ConsentAuditError::Validation(
                "document_ceiling_raised requires a document_type".to_owned(),
            ))
        }
        (false, Some(_)) => {
            return Err(ConsentAuditError::Validation(
                "document_type is only for document_ceiling_raised".to_owned(),
            ))
        }
        _ => {}
    }

    let id = uuid::Uuid::new_v4().to_string();
    let created_at = crate::providers::utils::now();
    sqlx::query(
        "INSERT INTO consent_audit_records
         (id, event_type, focus_slug, run_id, text_version, consent_reference_id,
          content_summary, document_type, created_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(event.event_type.as_str())
    .bind(event.focus_slug)
    .bind(event.run_id)
    .bind(event.text_version)
    .bind(event.consent_reference_id)
    .bind(event.content_summary)
    .bind(event.document_type)
    .bind(&created_at)
    .execute(&mut *conn)
    .await?;
    Ok(id)
}

/// Every event of one Focus, oldest first.
pub async fn list(
    conn: &mut SqliteConnection,
    focus_slug: &str,
) -> Result<Vec<ConsentAuditRecord>, ConsentAuditError> {
    let rows = sqlx::query(&format!(
        "SELECT {COLUMNS} FROM consent_audit_records
         WHERE focus_slug = ? ORDER BY created_at ASC, rowid ASC"
    ))
    .bind(focus_slug)
    .fetch_all(&mut *conn)
    .await?;
    rows.iter().map(row_to_record).collect()
}

/// The newest event of one Focus, or None.
pub async fn latest_for_focus(
    conn: &mut SqliteConnection,
    focus_slug: &str,
) -> Result<Option<ConsentAuditRecord>, ConsentAuditError> {
    let row = sqlx::query(&format!(
        "SELECT {COLUMNS} FROM consent_audit_records
         WHERE focus_slug = ? ORDER BY created_at DESC, rowid DESC LIMIT 1"
    ))
    .bind(focus_slug)
    .fetch_optional(&mut *conn)
    .await?;
    row.as_ref().map(row_to_record).transpose()
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

    const PERSONAL_SCHEMA_V1: &str = include_str!("../../schema/personal_001.sql");
    const PERSONAL_SCHEMA_V10: &str = include_str!("../../schema/personal_010.sql");

    async fn test_db() -> SqliteConnection {
        let mut conn = SqliteConnectOptions::new()
            .filename(":memory:")
            .connect()
            .await
            .expect("in-memory connection failed");
        for stmt in parse_statements(PERSONAL_SCHEMA_V1)
            .into_iter()
            .chain(parse_statements(PERSONAL_SCHEMA_V10))
        {
            sqlx::query(&stmt)
                .execute(&mut conn)
                .await
                .unwrap_or_else(|e| panic!("schema statement failed: {e}\n{stmt}"));
        }
        conn
    }

    fn event(t: ConsentEventType) -> NewConsentEvent<'static> {
        NewConsentEvent {
            event_type: t,
            focus_slug: "tech-support",
            run_id: None,
            text_version: Some(1),
            consent_reference_id: None,
            content_summary: "A fixed description.",
            document_type: None,
        }
    }

    #[test]
    fn event_types_round_trip_and_are_distinct() {
        let mut seen = std::collections::HashSet::new();
        for t in ConsentEventType::ALL {
            assert_eq!(ConsentEventType::parse(t.as_str()), Some(t), "{t:?}");
            assert!(seen.insert(t.as_str()), "duplicate string {}", t.as_str());
        }
        assert_eq!(ConsentEventType::parse("nope"), None);
        // The strings the schema header and decisions.id=434 name.
        for s in [
            "collection_consent_granted",
            "collection_consent_withdrawn",
            "document_ceiling_raised",
            "tier3_generation",
            "tier3_validation",
            "sensitivity_override",
            "standing_consent_use",
            "output_privacy_disclosure",
        ] {
            assert!(ConsentEventType::parse(s).is_some(), "{s}");
        }
        assert_eq!(ConsentEventType::ALL.len(), 8);
    }

    #[tokio::test]
    async fn every_event_type_round_trips_through_the_table() {
        let mut conn = test_db().await;
        for t in ConsentEventType::ALL {
            let mut e = event(t);
            if t == ConsentEventType::DocumentCeilingRaised {
                e.document_type = Some("cover_letter");
            }
            let id = append(&mut conn, &e).await.unwrap();
            let all = list(&mut conn, "tech-support").await.unwrap();
            let got = all.iter().find(|r| r.id == id).expect("row listed");
            assert_eq!(got.event_type, t);
            assert_eq!(got.text_version, Some(1));
            assert_eq!(got.run_id, None);
        }
    }

    #[tokio::test]
    async fn a_chain_lists_oldest_first_and_latest_is_the_newest() {
        let mut conn = test_db().await;
        assert!(latest_for_focus(&mut conn, "tech-support")
            .await
            .unwrap()
            .is_none());
        let first = append(
            &mut conn,
            &event(ConsentEventType::CollectionConsentGranted),
        )
        .await
        .unwrap();
        let mut second = event(ConsentEventType::CollectionConsentGranted);
        second.consent_reference_id = Some(&first);
        second.text_version = Some(2);
        let second_id = append(&mut conn, &second).await.unwrap();
        let mut other = event(ConsentEventType::CollectionConsentGranted);
        other.focus_slug = "other-focus";
        append(&mut conn, &other).await.unwrap();

        let all = list(&mut conn, "tech-support").await.unwrap();
        assert_eq!(all.len(), 2, "scoped to the Focus");
        assert_eq!(all[0].id, first);
        assert_eq!(all[1].id, second_id);
        assert_eq!(all[1].consent_reference_id.as_deref(), Some(first.as_str()));
        let latest = latest_for_focus(&mut conn, "tech-support")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(latest.id, second_id);
        assert_eq!(latest.text_version, Some(2));
    }

    #[tokio::test]
    async fn append_validates_its_arguments() {
        let mut conn = test_db().await;
        let mut e = event(ConsentEventType::DocumentCeilingRaised);
        assert!(matches!(
            append(&mut conn, &e).await,
            Err(ConsentAuditError::Validation(_))
        ));
        e.document_type = Some("letter");
        append(&mut conn, &e).await.unwrap();
        let mut e = event(ConsentEventType::CollectionConsentGranted);
        e.document_type = Some("letter");
        assert!(matches!(
            append(&mut conn, &e).await,
            Err(ConsentAuditError::Validation(_))
        ));
        let mut e = event(ConsentEventType::CollectionConsentGranted);
        e.content_summary = " ";
        assert!(matches!(
            append(&mut conn, &e).await,
            Err(ConsentAuditError::Validation(_))
        ));
        let mut e = event(ConsentEventType::CollectionConsentGranted);
        e.focus_slug = "";
        assert!(matches!(
            append(&mut conn, &e).await,
            Err(ConsentAuditError::Validation(_))
        ));
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM consent_audit_records")
                .fetch_one(&mut conn)
                .await
                .unwrap(),
            1
        );
    }

    /// The table is permanent: this module has no UPDATE and no DELETE (and no
    /// REPLACE or DROP), so a row once appended is never changed here. A purge
    /// is a separate user-initiated function, not an addition to this module.
    #[test]
    fn this_module_never_updates_or_deletes_a_row() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src/persistence/consent_audit_store.rs");
        let text = std::fs::read_to_string(path).expect("source readable");
        let production = text.split("#[cfg(test)]").next().unwrap().to_lowercase();
        for forbidden in [
            "update consent_audit_records",
            "delete from",
            "replace into",
            "insert or replace",
            "drop table",
            "alter table",
            "truncate",
            "pub async fn delete",
            "pub async fn update",
            "pub async fn purge",
        ] {
            let hits: Vec<_> = production
                .lines()
                .filter(|l| !l.trim_start().starts_with("//"))
                .filter(|l| l.contains(forbidden))
                .collect();
            assert!(hits.is_empty(), "`{forbidden}`: {hits:?}");
        }
        // The only public functions are the three the item allows.
        let fns: Vec<&str> = production
            .lines()
            .filter(|l| l.trim_start().starts_with("pub async fn "))
            .map(str::trim)
            .collect();
        assert_eq!(fns.len(), 3, "{fns:?}");
    }
}
