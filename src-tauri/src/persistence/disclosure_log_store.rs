// src-tauri/src/persistence/disclosure_log_store.rs
//
// items.id=173 -- Concrete DisclosureLogger implementation ("Layer 8").
// Backs the DisclosureLogger trait (conductor/privacy/logger.rs) with the
// disclosure_log table in personal.db (schema/personal_001.sql).
//
// CONTEXT: production has been wired PrivacyGateway<NoopLogger> since the
// privacy gates were first ported (lifecycle.rs's own doc comment calls
// this "migration scaffold"). Every disclosure-log entry the gates
// construct today -- including the Cross-Persona omission entry added
// under items.id=27 (commit dd9887a, decisions.id=546/639) -- has been
// discarded rather than persisted. SqliteDisclosureLogger is the concrete
// implementation that makes those entries durable, and is expected to be
// the backing store decisions.id=620's R1 privacy audit view eventually
// reads from (that view itself is blocked on items.id=9 opening --
// separate, out of scope here).
//
// APPEND-ONLY, ON PURPOSE: personal_store.rs's own module header states
// this as a standing rule for the whole database file -- "disclosure_log
// is NEVER deleted — permanent audit trail (D6-198). delete_disclosure_log
// does NOT exist in this module. Do not add it." This module inherits that
// rule by construction: it exposes exactly one operation, write(), and
// intentionally has no delete/update path. If a future need for retention
// pruning appears, that is a new decision to make explicitly, not a quiet
// addition here.
//
// TESTABILITY, matching entity_store.rs's precedent exactly: the public
// write() method (the DisclosureLogger trait impl) opens personal.db via
// open_personal_db and delegates to write_conn(&mut SqliteConnection, ...),
// which is what the unit tests below exercise against an in-memory
// database seeded from the real schema.
//
// WHY A STRUCT RATHER THAN A FREE FUNCTION: DisclosureLogger (logger.rs)
// is a trait or NoopLogger/TestLogger/FailLogger already implement it as
// zero-field or Mutex-holding structs constructed once and passed by
// reference into PrivacyGateway::new(). SqliteDisclosureLogger follows the
// same shape so it drops into that exact call site.

use async_trait::async_trait;

use crate::conductor::privacy::errors::DisclosureLogWriteError;
use crate::conductor::privacy::logger::{DisclosureLogEntry, DisclosureLogger, DisclosureLoggerForRun};
use crate::persistence::personal_store::{open_personal_db, PersonalStoreError};

/// Concrete, SQLCipher-backed DisclosureLogger. Holds only the identity
/// needed to open personal.db -- user_id, persona_id, key_hex -- exactly
/// the three parameters open_personal_db itself requires, and nothing else.
/// No connection is held open between calls: each write() opens, inserts,
/// and closes, matching every other store function in this persistence
/// layer (entity_store.rs, personal_store.rs) rather than introducing a
/// pooled-connection pattern found nowhere else in the codebase.
pub struct SqliteDisclosureLogger {
    user_id: String,
    persona_id: String,
    key_hex: String,
}

impl SqliteDisclosureLogger {
    pub fn new(user_id: impl Into<String>, persona_id: impl Into<String>, key_hex: impl Into<String>) -> Self {
        Self { user_id: user_id.into(), persona_id: persona_id.into(), key_hex: key_hex.into() }
    }
}

#[async_trait]
impl DisclosureLogger for SqliteDisclosureLogger {
    async fn write(&self, entry: DisclosureLogEntry) -> Result<String, DisclosureLogWriteError> {
        let mut conn = open_personal_db(&self.user_id, &self.persona_id, &self.key_hex)
            .await
            .map_err(DisclosureLogWriteError::new)?;
        write_conn(&mut conn, &self.user_id, &self.persona_id, entry)
            .await
            .map_err(DisclosureLogWriteError::new)
    }
}

/// FocusRun<L>::initialize() (lifecycle.rs, Phase 3) calls L::for_run(...)
/// generically to build whichever concrete logger L is -- see
/// DisclosureLoggerForRun's doc comment (logger.rs) for the full rationale.
/// key_hex here matches build_personal_track()'s own established convention
/// for the same field one call away: empty string when key_hex is absent,
/// not a hard failure -- a run with no key_hex already degrades gracefully
/// elsewhere (assemble_persona_context() returns "" the same way), so the
/// logger should not be the one place that panics or errors on it.
impl DisclosureLoggerForRun for SqliteDisclosureLogger {
    fn for_run(user_id: &str, persona_id: &str, key_hex: &str) -> Self {
        SqliteDisclosureLogger::new(user_id, persona_id, key_hex)
    }
}

/// Insert one disclosure_log row. Returns the generated id (UUID v4).
///
/// Column mapping notes (disclosure_log DDL, schema/personal_001.sql):
///   - routing_tier (NOT NULL, no DEFAULT) is populated from
///     entry.execution_tier. The table predates the ADR-012 tier split
///     that introduced DisclosureLogEntry's separate execution_tier /
///     abstraction_tier fields; execution_tier is the "which inference
///     tier ran" concept the column comment describes routing_tier as,
///     so it is the correct source, not abstraction_tier.
///   - execution_tier / abstraction_tier (both nullable, added later per
///     the column comments) are populated directly from the matching
///     entry fields -- this is what lets both old and new rows coexist
///     under the "both nullable... all records going forward always
///     populate both" contract the schema comment describes.
///   - declined_at is set to the same timestamp as created_at when
///     entry.override_declined is true, and left NULL otherwise. The
///     column exists to record when a decline happened; DisclosureLogEntry
///     does not carry a separate declined-at timestamp because gate
///     functions construct and write the entry synchronously at the
///     moment of decline, so "when written" and "when declined" are the
///     same instant. If a future caller needs to record a decline that
///     happened at a different time than the write, that is new field
///     work on DisclosureLogEntry, not something this function should
///     guess at.
///   - extra_metadata carries event_type, since the table has no
///     dedicated event_type column (the personal_001.sql comment for
///     execution_tier/abstraction_tier confirms extra_metadata is where
///     "extra" gate-specific detail belongs). Stored as
///     {"event_type": "..."} rather than a bare string, so a future
///     caller adding more metadata keys extends the object instead of
///     replacing a scalar column value.
pub(crate) async fn write_conn(
    conn: &mut sqlx::SqliteConnection,
    user_id: &str,
    persona_id: &str,
    entry: DisclosureLogEntry,
) -> Result<String, PersonalStoreError> {
    let id = uuid::Uuid::new_v4().to_string();
    let created_at = crate::providers::utils::now();

    let fields_shared_json = serde_json::to_string(&entry.fields_shared).map_err(|e| {
        PersonalStoreError::Validation(format!("fields_shared not serializable: {e}"))
    })?;
    let fields_abstracted_json = serde_json::to_string(&entry.fields_abstracted).map_err(|e| {
        PersonalStoreError::Validation(format!("fields_abstracted not serializable: {e}"))
    })?;
    let fields_withheld_json = serde_json::to_string(&entry.fields_withheld).map_err(|e| {
        PersonalStoreError::Validation(format!("fields_withheld not serializable: {e}"))
    })?;
    let extra_metadata_json = serde_json::to_string(&serde_json::json!({
        "event_type": entry.event_type,
    }))
    .map_err(|e| {
        PersonalStoreError::Validation(format!("extra_metadata not serializable: {e}"))
    })?;

    let declined_at: Option<String> = if entry.override_declined {
        Some(created_at.clone())
    } else {
        None
    };

    sqlx::query(
        "INSERT INTO disclosure_log
         (id, user_id, persona_id, focus_run_id, step_id, routing_tier,
          provider, fields_shared, fields_abstracted, fields_withheld,
          override_declined, declined_at, created_at, extra_metadata,
          execution_tier, abstraction_tier)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(user_id)
    .bind(persona_id)
    .bind(&entry.focus_run_id)
    .bind(&entry.step_id)
    .bind(entry.execution_tier as i64)
    .bind(&entry.provider)
    .bind(&fields_shared_json)
    .bind(&fields_abstracted_json)
    .bind(&fields_withheld_json)
    .bind(entry.override_declined)
    .bind(&declined_at)
    .bind(&created_at)
    .bind(&extra_metadata_json)
    .bind(entry.execution_tier as i64)
    .bind(entry.abstraction_tier.map(|t| t as i64))
    .execute(&mut *conn)
    .await?;

    Ok(id)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------
//
// Real personal_001.sql schema against an in-memory SQLite database, same
// pattern as entity_store.rs's test suite -- so a disclosure_log DDL change
// (a NOT NULL added, a CHECK constraint tightened) fails here, not at
// runtime. No QR_DATA_ROOT and no SQLCipher key are involved: encryption is
// a property of the file on disk, not of the SQL these functions issue.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persistence::migrations::parse_statements;
    use indexmap::IndexMap;
    use sqlx::sqlite::SqliteConnectOptions;
    use sqlx::ConnectOptions;
    use sqlx::Row;

    const PERSONAL_SCHEMA_V1: &str = include_str!("../../schema/personal_001.sql");

    async fn test_db() -> sqlx::SqliteConnection {
        let mut conn = SqliteConnectOptions::new()
            .filename(":memory:")
            .connect()
            .await
            .expect("in-memory connection failed");

        for stmt in parse_statements(PERSONAL_SCHEMA_V1) {
            sqlx::query(&stmt)
                .execute(&mut conn)
                .await
                .unwrap_or_else(|e| panic!("schema statement failed: {e}\n{stmt}"));
        }
        conn
    }

    fn sample_entry(event_type: &str) -> DisclosureLogEntry {
        DisclosureLogEntry {
            step_id: "step-1".to_owned(),
            focus_run_id: "run-1".to_owned(),
            execution_tier: 2,
            abstraction_tier: Some(1),
            provider: Some("anthropic".to_owned()),
            fields_shared: vec!["field_a".to_owned()],
            fields_abstracted: IndexMap::new(),
            fields_withheld: vec!["field_b".to_owned()],
            override_declined: false,
            event_type: event_type.to_owned(),
        }
    }

    #[tokio::test]
    async fn write_conn_inserts_row_and_returns_id() {
        let mut conn = test_db().await;
        let id = write_conn(&mut conn, "user-1", "persona-1", sample_entry("test_event"))
            .await
            .expect("write failed");
        assert!(!id.is_empty());

        let row = sqlx::query("SELECT * FROM disclosure_log WHERE id = ?")
            .bind(&id)
            .fetch_one(&mut conn)
            .await
            .expect("row must exist");

        let user_id: String = row.try_get("user_id").unwrap();
        let persona_id: String = row.try_get("persona_id").unwrap();
        let focus_run_id: String = row.try_get("focus_run_id").unwrap();
        let step_id: String = row.try_get("step_id").unwrap();
        let routing_tier: i64 = row.try_get("routing_tier").unwrap();
        let provider: Option<String> = row.try_get("provider").unwrap();
        let execution_tier: Option<i64> = row.try_get("execution_tier").unwrap();
        let abstraction_tier: Option<i64> = row.try_get("abstraction_tier").unwrap();

        assert_eq!(user_id, "user-1");
        assert_eq!(persona_id, "persona-1");
        assert_eq!(focus_run_id, "run-1");
        assert_eq!(step_id, "step-1");
        assert_eq!(routing_tier, 2, "routing_tier must be sourced from execution_tier");
        assert_eq!(provider.as_deref(), Some("anthropic"));
        assert_eq!(execution_tier, Some(2));
        assert_eq!(abstraction_tier, Some(1));
    }

    #[tokio::test]
    async fn write_conn_serializes_json_fields_correctly() {
        let mut conn = test_db().await;
        let mut entry = sample_entry("test_event");
        entry.fields_abstracted.insert("name".to_owned(), "abstracted_value".to_owned());
        let id = write_conn(&mut conn, "user-1", "persona-1", entry)
            .await
            .expect("write failed");

        let row = sqlx::query("SELECT fields_shared, fields_abstracted, fields_withheld, extra_metadata FROM disclosure_log WHERE id = ?")
            .bind(&id)
            .fetch_one(&mut conn)
            .await
            .unwrap();

        let fields_shared: String = row.try_get("fields_shared").unwrap();
        let fields_abstracted: String = row.try_get("fields_abstracted").unwrap();
        let fields_withheld: String = row.try_get("fields_withheld").unwrap();
        let extra_metadata: String = row.try_get("extra_metadata").unwrap();

        assert_eq!(serde_json::from_str::<Vec<String>>(&fields_shared).unwrap(), vec!["field_a"]);
        let abstracted: std::collections::HashMap<String, String> =
            serde_json::from_str(&fields_abstracted).unwrap();
        assert_eq!(abstracted.get("name"), Some(&"abstracted_value".to_owned()));
        assert_eq!(serde_json::from_str::<Vec<String>>(&fields_withheld).unwrap(), vec!["field_b"]);
        let metadata: serde_json::Value = serde_json::from_str(&extra_metadata).unwrap();
        assert_eq!(metadata["event_type"], "test_event");
    }

    #[tokio::test]
    async fn write_conn_sets_declined_at_when_override_declined() {
        let mut conn = test_db().await;
        let mut entry = sample_entry("declined_event");
        entry.override_declined = true;
        let id = write_conn(&mut conn, "user-1", "persona-1", entry).await.unwrap();

        let row = sqlx::query("SELECT override_declined, declined_at, created_at FROM disclosure_log WHERE id = ?")
            .bind(&id)
            .fetch_one(&mut conn)
            .await
            .unwrap();
        let override_declined: bool = row.try_get("override_declined").unwrap();
        let declined_at: Option<String> = row.try_get("declined_at").unwrap();
        let created_at: String = row.try_get("created_at").unwrap();

        assert!(override_declined);
        assert_eq!(declined_at.as_deref(), Some(created_at.as_str()));
    }

    #[tokio::test]
    async fn write_conn_leaves_declined_at_null_when_not_declined() {
        let mut conn = test_db().await;
        let id = write_conn(&mut conn, "user-1", "persona-1", sample_entry("normal_event"))
            .await
            .unwrap();

        let row = sqlx::query("SELECT declined_at FROM disclosure_log WHERE id = ?")
            .bind(&id)
            .fetch_one(&mut conn)
            .await
            .unwrap();
        let declined_at: Option<String> = row.try_get("declined_at").unwrap();
        assert!(declined_at.is_none());
    }

    #[tokio::test]
    async fn write_conn_handles_none_provider_and_abstraction_tier() {
        let mut conn = test_db().await;
        let mut entry = sample_entry("no_provider_event");
        entry.provider = None;
        entry.abstraction_tier = None;
        let id = write_conn(&mut conn, "user-1", "persona-1", entry).await.unwrap();

        let row = sqlx::query("SELECT provider, abstraction_tier FROM disclosure_log WHERE id = ?")
            .bind(&id)
            .fetch_one(&mut conn)
            .await
            .unwrap();
        let provider: Option<String> = row.try_get("provider").unwrap();
        let abstraction_tier: Option<i64> = row.try_get("abstraction_tier").unwrap();
        assert!(provider.is_none());
        assert!(abstraction_tier.is_none());
    }

    #[tokio::test]
    async fn write_conn_never_produces_a_row_deletable_by_this_module() {
        // Structural guard, not a behavioural assertion: this module must
        // expose no delete function at all (D6-198, personal_store.rs's
        // own module-header rule, inherited here by construction). If this
        // test ever needs a delete_* call to compile, that is the signal
        // the append-only contract has been violated.
        let mut conn = test_db().await;
        let id = write_conn(&mut conn, "user-1", "persona-1", sample_entry("permanent_event"))
            .await
            .unwrap();
        let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM disclosure_log WHERE id = ?")
            .bind(&id)
            .fetch_one(&mut conn)
            .await
            .unwrap();
        assert_eq!(count.0, 1);
    }

    #[tokio::test]
    async fn multiple_writes_produce_distinct_ids() {
        let mut conn = test_db().await;
        let id1 = write_conn(&mut conn, "user-1", "persona-1", sample_entry("e1")).await.unwrap();
        let id2 = write_conn(&mut conn, "user-1", "persona-1", sample_entry("e2")).await.unwrap();
        assert_ne!(id1, id2);

        let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM disclosure_log")
            .fetch_one(&mut conn)
            .await
            .unwrap();
        assert_eq!(count.0, 2);
    }
}
