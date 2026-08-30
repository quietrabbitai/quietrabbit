// src-tauri/src/persistence/view_cache_store.rs
//
// items.id=304 (decisions.id=723): CRUD against view_cache.db -- VIEW-ONLY
// persona sharing's read-only recipient cache. See schema/view_cache_001.sql's
// own header for the full placement/shape reasoning. This module is
// deliberately small: open, replace-content-wholesale, mark-ended, read-back
// -- there is no update-in-place, no per-row write, no modification_state,
// because nothing here is ever locally edited (decisions.id=723).
//
// Mirrors personal_store.rs::open_personal_db's exact opener shape (PRAGMA
// key before journal_mode, x'...' blob-literal form, migrate-on-first-open)
// -- same account master-key hex, only the path and schema family differ
// (persistence::migrations::migrate_view_cache_db).
//
// QUERY STYLE: runtime sqlx::query() only -- no query!() macros, matching
// the rest of this codebase.

use std::path::PathBuf;

use sqlx::sqlite::SqliteConnectOptions;
use sqlx::ConnectOptions;
use sqlx::Row;
use sqlx::SqliteConnection;
use thiserror::Error;

use crate::auth::persona_sharing::{SharedEntityFact, SharedVoiceProfileEntry};
use crate::persistence::entity_store::Entity;

#[derive(Debug, Error)]
pub enum ViewCacheStoreError {
    #[error("Database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("Migration error: {0}")]
    Migration(#[from] crate::persistence::migrations::MigrationError),
}

// ---------------------------------------------------------------------------
// Data types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ViewCacheStatus {
    Active,
    Ended,
}

impl ViewCacheStatus {
    fn parse(s: &str) -> Self {
        match s {
            "ended" => ViewCacheStatus::Ended,
            _ => ViewCacheStatus::Active,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewCacheMeta {
    pub share_id: String,
    pub source_persona_display_name: String,
    pub source_persona_type: String,
    pub status: ViewCacheStatus,
    pub last_synced_at: Option<String>,
    pub ended_at: Option<String>,
}

fn row_to_meta(r: &sqlx::sqlite::SqliteRow) -> Result<ViewCacheMeta, ViewCacheStoreError> {
    let status: String = r.try_get("status")?;
    Ok(ViewCacheMeta {
        share_id: r.try_get("share_id")?,
        source_persona_display_name: r.try_get("source_persona_display_name")?,
        source_persona_type: r.try_get("source_persona_type")?,
        status: ViewCacheStatus::parse(&status),
        last_synced_at: r.try_get("last_synced_at")?,
        ended_at: r.try_get("ended_at")?,
    })
}

// ---------------------------------------------------------------------------
// DB opener
// ---------------------------------------------------------------------------

fn get_view_cache_db_path(user_id: &str, share_id: &str) -> PathBuf {
    crate::persistence::migrations::get_data_root()
        .join("users")
        .join(user_id)
        .join("persona_view_shares")
        .join(share_id)
        .join("view_cache.db")
}

/// Open view_cache.db with SQLCipher key, migrating on first open -- same
/// shape as personal_store::open_personal_db. `key_hex`: the recipient
/// account's own master-key hex (KeyRegistry::personal_key_hex) -- the same
/// key that already encrypts every personal.db under this account.
pub(crate) async fn open_view_cache_db(
    user_id: &str,
    share_id: &str,
    key_hex: &str,
) -> Result<SqliteConnection, ViewCacheStoreError> {
    let db_path = get_view_cache_db_path(user_id, share_id);

    if !db_path.exists() {
        crate::persistence::migrations::migrate_view_cache_db(user_id, share_id, key_hex).await?;
    }

    let network_storage = std::env::var("QR_NETWORK_STORAGE")
        .map(|v| v.to_lowercase() == "true")
        .unwrap_or(false);
    let journal_mode = if network_storage { "DELETE" } else { "WAL" };

    let conn = SqliteConnectOptions::new()
        .filename(&db_path)
        .create_if_missing(false)
        .pragma("key", format!("\"x'{key_hex}'\""))
        .pragma("cipher_compatibility", "4")
        .pragma("journal_mode", journal_mode)
        .connect()
        .await?;

    Ok(conn)
}

// ---------------------------------------------------------------------------
// Write: initial accept
// ---------------------------------------------------------------------------

/// First-write of the singleton meta row, at accept time. Idempotent via
/// INSERT OR IGNORE (id is CHECK-pinned to 1) -- a retried accept after a
/// partial failure elsewhere is safe.
pub(crate) async fn init_meta_conn(
    conn: &mut SqliteConnection,
    share_id: &str,
    source_persona_display_name: &str,
    source_persona_type: &str,
) -> Result<(), ViewCacheStoreError> {
    sqlx::query(
        "INSERT OR IGNORE INTO view_cache_meta
            (id, share_id, source_persona_display_name, source_persona_type, status)
         VALUES (1, ?, ?, ?, 'active')",
    )
    .bind(share_id)
    .bind(source_persona_display_name)
    .bind(source_persona_type)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Write: wholesale replace (every applied Content payload)
// ---------------------------------------------------------------------------

/// Replace every cached entity/fact/voice-profile row wholesale with the
/// given snapshot, and stamp last_synced_at -- decisions.id=723's "no merge,
/// nothing recipient-editable" contract. Caller wraps this in a SAVEPOINT
/// (persona_view_sync::engine::apply_content) -- this fn issues raw
/// DELETE+INSERT statements only, no transaction control of its own, so it
/// composes cleanly with init_meta_conn in one caller-owned SAVEPOINT.
pub(crate) async fn replace_content_conn(
    conn: &mut SqliteConnection,
    entities: &[Entity],
    entity_facts: &[SharedEntityFact],
    voice_profile_entries: &[SharedVoiceProfileEntry],
    emitted_at: &str,
) -> Result<(), ViewCacheStoreError> {
    // Facts and entities are deleted together, facts first -- entity_id has
    // an ON DELETE CASCADE FK to entities, but being explicit here means
    // this fn's behavior doesn't depend on that FK enforcement being on.
    sqlx::query("DELETE FROM view_cache_entity_facts")
        .execute(&mut *conn)
        .await?;
    sqlx::query("DELETE FROM view_cache_entities")
        .execute(&mut *conn)
        .await?;
    sqlx::query("DELETE FROM view_cache_voice_profile_entries")
        .execute(&mut *conn)
        .await?;

    for entity in entities {
        let aliases_json =
            serde_json::to_string(&entity.aliases).unwrap_or_else(|_| "[]".to_owned());
        let extra_metadata_json = entity.extra_metadata.to_string();
        sqlx::query(
            "INSERT INTO view_cache_entities
                (id, entity_type, display_name, aliases, parent_entity_id, status,
                 source_url, created_at, extra_metadata, redact_identification,
                 hide_from_shared_surfaces)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&entity.id)
        .bind(&entity.entity_type)
        .bind(&entity.display_name)
        .bind(&aliases_json)
        .bind(&entity.parent_entity_id)
        .bind(&entity.status)
        .bind(&entity.source_url)
        .bind(&entity.created_at)
        .bind(&extra_metadata_json)
        .bind(entity.redact_identification)
        .bind(entity.hide_from_shared_surfaces)
        .execute(&mut *conn)
        .await?;
    }

    for fact in entity_facts {
        let extra_metadata_json = fact.extra_metadata.to_string();
        sqlx::query(
            "INSERT INTO view_cache_entity_facts
                (id, entity_id, field_name, field_value, sensitivity,
                 abstraction_tier2, abstraction_tier3, source, valid_from,
                 created_at, extra_metadata)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&fact.id)
        .bind(&fact.entity_id)
        .bind(&fact.field_name)
        .bind(&fact.field_value)
        .bind(&fact.sensitivity)
        .bind(&fact.abstraction_tier2)
        .bind(&fact.abstraction_tier3)
        .bind(&fact.source)
        .bind(&fact.valid_from)
        .bind(&fact.created_at)
        .bind(&extra_metadata_json)
        .execute(&mut *conn)
        .await?;
    }

    for entry in voice_profile_entries {
        let extra_metadata_json = entry.extra_metadata.to_string();
        sqlx::query(
            "INSERT INTO view_cache_voice_profile_entries
                (id, source_id, precedence, attribute, value, created_at,
                 updated_at, extra_metadata)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&entry.id)
        .bind(&entry.source_id)
        .bind(entry.precedence)
        .bind(&entry.attribute)
        .bind(&entry.value)
        .bind(&entry.created_at)
        .bind(&entry.updated_at)
        .bind(&extra_metadata_json)
        .execute(&mut *conn)
        .await?;
    }

    sqlx::query("UPDATE view_cache_meta SET status = 'active', last_synced_at = ?")
        .bind(emitted_at)
        .execute(&mut *conn)
        .await?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Write: tombstone
// ---------------------------------------------------------------------------

/// Apply a Revoked tombstone -- delete every cached content row and mark the
/// share permanently ended. A positive, terminal state (decisions.id=723:
/// "not silent staleness"), not merely an absence of further updates.
pub(crate) async fn mark_ended_conn(
    conn: &mut SqliteConnection,
    emitted_at: &str,
) -> Result<(), ViewCacheStoreError> {
    sqlx::query("DELETE FROM view_cache_entity_facts")
        .execute(&mut *conn)
        .await?;
    sqlx::query("DELETE FROM view_cache_entities")
        .execute(&mut *conn)
        .await?;
    sqlx::query("DELETE FROM view_cache_voice_profile_entries")
        .execute(&mut *conn)
        .await?;

    sqlx::query("UPDATE view_cache_meta SET status = 'ended', ended_at = ?, last_synced_at = ?")
        .bind(emitted_at)
        .bind(emitted_at)
        .execute(&mut *conn)
        .await?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Read
// ---------------------------------------------------------------------------

pub(crate) async fn get_meta_conn(
    conn: &mut SqliteConnection,
) -> Result<Option<ViewCacheMeta>, ViewCacheStoreError> {
    let row = sqlx::query(
        "SELECT share_id, source_persona_display_name, source_persona_type, status,
                last_synced_at, ended_at
         FROM view_cache_meta WHERE id = 1",
    )
    .fetch_optional(&mut *conn)
    .await?;

    match row {
        None => Ok(None),
        Some(r) => Ok(Some(row_to_meta(&r)?)),
    }
}

/// Count of cached entities -- test/verification helper for "wholesale
/// replace actually replaced, not appended".
#[cfg(test)]
pub(crate) async fn count_entities_conn(
    conn: &mut SqliteConnection,
) -> Result<i64, ViewCacheStoreError> {
    let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM view_cache_entities")
        .fetch_one(&mut *conn)
        .await?;
    Ok(count)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::ENV_MUTEX;

    struct TestEnv {
        _tempdir: tempfile::TempDir,
        _lock: std::sync::MutexGuard<'static, ()>,
        saved_root: Option<String>,
    }

    impl Drop for TestEnv {
        fn drop(&mut self) {
            match &self.saved_root {
                Some(v) => std::env::set_var("QR_DATA_ROOT", v),
                None => std::env::remove_var("QR_DATA_ROOT"),
            }
        }
    }

    async fn setup() -> TestEnv {
        let lock = ENV_MUTEX.lock().unwrap();
        let saved_root = std::env::var("QR_DATA_ROOT").ok();
        let tempdir = tempfile::tempdir().expect("failed to create tempdir");
        std::env::set_var("QR_DATA_ROOT", tempdir.path());
        TestEnv {
            _tempdir: tempdir,
            _lock: lock,
            saved_root,
        }
    }

    fn test_key_hex() -> String {
        [0x11u8; 32].iter().map(|b| format!("{b:02x}")).collect()
    }

    fn sample_entity(id: &str) -> Entity {
        Entity {
            id: id.to_owned(),
            entity_type: "person".to_owned(),
            display_name: "Alex".to_owned(),
            aliases: vec![],
            parent_entity_id: None,
            status: "active".to_owned(),
            modification_state: "pristine".to_owned(),
            source_registry_id: None,
            source_url: None,
            created_at: "2026-08-21T00:00:00Z".to_owned(),
            extra_metadata: serde_json::json!({}),
            redact_identification: false,
            hide_from_shared_surfaces: false,
        }
    }

    #[tokio::test]
    async fn open_migrates_on_first_open_and_meta_starts_absent() {
        let _env = setup().await;
        let key_hex = test_key_hex();
        let mut conn = open_view_cache_db("user-1", "share-1", &key_hex)
            .await
            .expect("open_view_cache_db must succeed");
        let meta = get_meta_conn(&mut conn)
            .await
            .expect("get_meta must succeed");
        assert!(meta.is_none());
    }

    #[tokio::test]
    async fn init_meta_then_replace_content_round_trips() {
        let _env = setup().await;
        let key_hex = test_key_hex();
        let mut conn = open_view_cache_db("user-1", "share-1", &key_hex)
            .await
            .unwrap();

        init_meta_conn(&mut conn, "share-1", "Household", "personal")
            .await
            .unwrap();

        let entities = vec![sample_entity("e1")];
        replace_content_conn(&mut conn, &entities, &[], &[], "2026-08-21T01:00:00Z")
            .await
            .expect("replace_content_conn must succeed");

        let meta = get_meta_conn(&mut conn)
            .await
            .unwrap()
            .expect("meta must exist after init");
        assert_eq!(meta.status, ViewCacheStatus::Active);
        assert_eq!(meta.last_synced_at.as_deref(), Some("2026-08-21T01:00:00Z"));
        assert_eq!(count_entities_conn(&mut conn).await.unwrap(), 1);
    }

    #[tokio::test]
    async fn replace_content_is_wholesale_not_additive() {
        let _env = setup().await;
        let key_hex = test_key_hex();
        let mut conn = open_view_cache_db("user-1", "share-1", &key_hex)
            .await
            .unwrap();
        init_meta_conn(&mut conn, "share-1", "Household", "personal")
            .await
            .unwrap();

        replace_content_conn(&mut conn, &[sample_entity("e1")], &[], &[], "t1")
            .await
            .unwrap();
        replace_content_conn(&mut conn, &[sample_entity("e2")], &[], &[], "t2")
            .await
            .unwrap();

        assert_eq!(
            count_entities_conn(&mut conn).await.unwrap(),
            1,
            "second replace must not leave e1 behind alongside e2"
        );
    }

    #[tokio::test]
    async fn mark_ended_clears_content_and_sets_terminal_status() {
        let _env = setup().await;
        let key_hex = test_key_hex();
        let mut conn = open_view_cache_db("user-1", "share-1", &key_hex)
            .await
            .unwrap();
        init_meta_conn(&mut conn, "share-1", "Household", "personal")
            .await
            .unwrap();
        replace_content_conn(&mut conn, &[sample_entity("e1")], &[], &[], "t1")
            .await
            .unwrap();

        mark_ended_conn(&mut conn, "t2")
            .await
            .expect("mark_ended_conn must succeed");

        let meta = get_meta_conn(&mut conn).await.unwrap().unwrap();
        assert_eq!(meta.status, ViewCacheStatus::Ended);
        assert_eq!(meta.ended_at.as_deref(), Some("t2"));
        assert_eq!(count_entities_conn(&mut conn).await.unwrap(), 0);
    }
}
