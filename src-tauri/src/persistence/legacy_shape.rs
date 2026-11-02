// src-tauri/src/persistence/legacy_shape.rs
//
// Detects a shared.db built before items.id=661 (decisions.id=862) moved
// private metadata out of it (the un-reset-database guard).
//
// WHY A GUARD: shared_001, 003 to 010, 013 and 026 were edited in place
// (decisions.id=857 allows that until first ship), and an edited v2+ file does
// not run again on a database that already applied it. A data directory created
// before the change still has the old tables and columns, and the new code
// would fail against it with NOT NULL and missing-table errors that say nothing
// about the cause. Worse, those databases still hold the names and paths in
// plaintext. So login refuses such a database with one clear message and the
// startup log names what was found. The remedy is a reset of the data
// directory (items.id=661, no data migration, Jason approved).
//
// This reports table and column NAMES only, never row content.

use sqlx::SqliteConnection;

/// What login shows. Neutral and factual, like the legacy-account message: it
/// says what is true of the data directory and that it has to be reset. Copy
/// for the user is Chat-BRAND's.
pub const LEGACY_PLAINTEXT_MESSAGE: &str =
    "This data directory predates the move of private names \
     and paths out of the unencrypted database and can't be used. It has to be reset";

/// Tables the move removed from shared.db.
const LEGACY_TABLES: &[&str] = &[
    "topic_index",
    "asset_index",
    "instance_context",
    "user_provider_preference",
    "group_sync_settings",
    "persona_share_sync_settings",
    "persona_view_share_settings",
];

/// (table, column) pairs the move removed from tables that remain.
const LEGACY_COLUMNS: &[(&str, &str)] = &[
    ("personas", "display_name"),
    ("personas", "persona_type"),
    ("personas", "extra_metadata"),
    ("pending_group_invitations", "group_display_name"),
    ("pending_group_invitations", "sender_label"),
    ("pending_group_key_rotations", "sender_label"),
    ("pending_persona_shares", "source_persona_display_name"),
    ("pending_persona_shares", "source_persona_type"),
];

/// Every pre-661 object present on `conn`, as "table" or "table.column".
/// Empty on a current database.
pub async fn legacy_plaintext_objects_conn(
    conn: &mut SqliteConnection,
) -> Result<Vec<String>, sqlx::Error> {
    let mut found = Vec::new();
    for table in LEGACY_TABLES {
        let row = sqlx::query("SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?")
            .bind(table)
            .fetch_optional(&mut *conn)
            .await?;
        if row.is_some() {
            found.push((*table).to_owned());
        }
    }
    for (table, column) in LEGACY_COLUMNS {
        let row = sqlx::query("SELECT 1 FROM pragma_table_info(?) WHERE name = ?")
            .bind(table)
            .bind(column)
            .fetch_optional(&mut *conn)
            .await?;
        if row.is_some() {
            found.push(format!("{table}.{column}"));
        }
    }
    Ok(found)
}

pub async fn legacy_plaintext_objects(pool: &sqlx::SqlitePool) -> Result<Vec<String>, sqlx::Error> {
    let mut conn = pool.acquire().await?;
    legacy_plaintext_objects_conn(&mut conn).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::SqliteConnectOptions;
    use sqlx::ConnectOptions;

    async fn memory() -> SqliteConnection {
        SqliteConnectOptions::new()
            .filename(":memory:")
            .connect()
            .await
            .expect("in-memory connection")
    }

    #[tokio::test]
    async fn a_current_database_reports_nothing() {
        let mut conn = memory().await;
        crate::persistence::migrations::run_migrations(&mut conn, "shared", None)
            .await
            .unwrap();
        assert_eq!(
            legacy_plaintext_objects_conn(&mut conn).await.unwrap(),
            Vec::<String>::new()
        );
    }

    /// A database in the pre-661 shape: every removed table and column is
    /// reported, by name, and only those.
    #[tokio::test]
    async fn a_pre_661_database_reports_every_removed_object() {
        let mut conn = memory().await;
        for ddl in [
            "CREATE TABLE personas (id TEXT PRIMARY KEY, display_name TEXT, persona_type TEXT, created_at TEXT, extra_metadata TEXT)",
            "CREATE TABLE pending_group_invitations (id TEXT PRIMARY KEY, group_display_name TEXT, sender_label TEXT)",
            "CREATE TABLE pending_group_key_rotations (id TEXT PRIMARY KEY, sender_label TEXT)",
            "CREATE TABLE pending_persona_shares (id TEXT PRIMARY KEY, source_persona_display_name TEXT, source_persona_type TEXT)",
            "CREATE TABLE topic_index (topic_id TEXT)",
            "CREATE TABLE asset_index (asset_id TEXT)",
            "CREATE TABLE instance_context (id TEXT)",
            "CREATE TABLE user_provider_preference (id TEXT)",
            "CREATE TABLE group_sync_settings (persona_id TEXT)",
            "CREATE TABLE persona_share_sync_settings (persona_id TEXT)",
            "CREATE TABLE persona_view_share_settings (share_id TEXT)",
        ] {
            sqlx::query(ddl).execute(&mut conn).await.unwrap();
        }
        let mut found = legacy_plaintext_objects_conn(&mut conn).await.unwrap();
        found.sort();
        let mut expected: Vec<String> = LEGACY_TABLES.iter().map(|t| (*t).to_owned()).collect();
        expected.extend(LEGACY_COLUMNS.iter().map(|(t, c)| format!("{t}.{c}")));
        expected.sort();
        assert_eq!(found, expected);
    }

    #[tokio::test]
    async fn one_leftover_column_is_enough() {
        let mut conn = memory().await;
        crate::persistence::migrations::run_migrations(&mut conn, "shared", None)
            .await
            .unwrap();
        sqlx::query("ALTER TABLE personas ADD COLUMN display_name TEXT")
            .execute(&mut conn)
            .await
            .unwrap();
        assert_eq!(
            legacy_plaintext_objects_conn(&mut conn).await.unwrap(),
            vec!["personas.display_name".to_owned()]
        );
    }
}
