// src-tauri/src/persistence/provider_catalog_store.rs
//
// State of the signed provider catalog in shared.db (items.id=713,
// shared_030.sql): which catalog version is in force, and the applied
// documents with their undo images. Every function takes a connection so the
// caller can run it inside the same transaction as the apply it records.

use sqlx::{Row, SqliteConnection};

/// Applied catalogs kept (the latest can be rolled back, then the one before).
pub const HISTORY_KEPT: i64 = 2;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogState {
    /// 0 = no catalog applied; the rows are as the release left them.
    pub applied_version: u64,
    pub applied_sha256: Option<String>,
    pub applied_at: Option<String>,
    /// The highest version the user rolled back; at or below it nothing is
    /// offered again.
    pub declined_version: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryRow {
    pub version: u64,
    pub sha256: String,
    pub document: String,
    pub undo: String,
    pub applied_at: String,
    pub rolled_back_at: Option<String>,
}

fn to_u64(v: i64) -> u64 {
    u64::try_from(v).unwrap_or(0)
}

fn to_i64(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

pub async fn get_state(conn: &mut SqliteConnection) -> Result<CatalogState, sqlx::Error> {
    let row = sqlx::query(
        "SELECT applied_version, applied_sha256, applied_at, declined_version
           FROM provider_catalog_state WHERE id = 1",
    )
    .fetch_one(&mut *conn)
    .await?;
    Ok(CatalogState {
        applied_version: to_u64(row.try_get("applied_version")?),
        applied_sha256: row.try_get("applied_sha256")?,
        applied_at: row.try_get("applied_at")?,
        declined_version: to_u64(row.try_get("declined_version")?),
    })
}

fn history_row(row: &sqlx::sqlite::SqliteRow) -> Result<HistoryRow, sqlx::Error> {
    Ok(HistoryRow {
        version: to_u64(row.try_get("version")?),
        sha256: row.try_get("sha256")?,
        document: row.try_get("document")?,
        undo: row.try_get("undo")?,
        applied_at: row.try_get("applied_at")?,
        rolled_back_at: row.try_get("rolled_back_at")?,
    })
}

/// The newest applied catalog that has not been rolled back.
pub async fn latest_history(
    conn: &mut SqliteConnection,
) -> Result<Option<HistoryRow>, sqlx::Error> {
    let row = sqlx::query(
        "SELECT version, sha256, document, undo, applied_at, rolled_back_at
           FROM provider_catalog_history
          WHERE rolled_back_at IS NULL
          ORDER BY version DESC LIMIT 1",
    )
    .fetch_optional(&mut *conn)
    .await?;
    row.as_ref().map(history_row).transpose()
}

pub async fn get_history(
    conn: &mut SqliteConnection,
    version: u64,
) -> Result<Option<HistoryRow>, sqlx::Error> {
    let row = sqlx::query(
        "SELECT version, sha256, document, undo, applied_at, rolled_back_at
           FROM provider_catalog_history WHERE version = ?",
    )
    .bind(to_i64(version))
    .fetch_optional(&mut *conn)
    .await?;
    row.as_ref().map(history_row).transpose()
}

/// Record an applied catalog and make it the version in force.
pub async fn record_apply(
    conn: &mut SqliteConnection,
    version: u64,
    sha256: &str,
    document: &str,
    undo_json: &str,
    now: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT OR REPLACE INTO provider_catalog_history
            (version, sha256, document, undo, applied_at, rolled_back_at)
         VALUES (?, ?, ?, ?, ?, NULL)",
    )
    .bind(to_i64(version))
    .bind(sha256)
    .bind(document)
    .bind(undo_json)
    .bind(now)
    .execute(&mut *conn)
    .await?;
    sqlx::query(
        "UPDATE provider_catalog_state
            SET applied_version = ?, applied_sha256 = ?, applied_at = ?
          WHERE id = 1",
    )
    .bind(to_i64(version))
    .bind(sha256)
    .bind(now)
    .execute(&mut *conn)
    .await?;
    sqlx::query(
        "DELETE FROM provider_catalog_history
          WHERE version NOT IN
                (SELECT version FROM provider_catalog_history
                  ORDER BY version DESC LIMIT ?)",
    )
    .bind(HISTORY_KEPT)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Mark `version` rolled back and move the state to the newest remaining
/// applied catalog (or to "none"). The rolled-back version is remembered so it
/// is not offered again.
pub async fn record_rollback(
    conn: &mut SqliteConnection,
    version: u64,
    now: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE provider_catalog_history SET rolled_back_at = ? WHERE version = ?")
        .bind(now)
        .bind(to_i64(version))
        .execute(&mut *conn)
        .await?;
    let prior = latest_history(conn).await?;
    match prior {
        Some(p) => {
            sqlx::query(
                "UPDATE provider_catalog_state
                    SET applied_version = ?, applied_sha256 = ?, applied_at = ?,
                        declined_version = MAX(declined_version, ?)
                  WHERE id = 1",
            )
            .bind(to_i64(p.version))
            .bind(&p.sha256)
            .bind(&p.applied_at)
            .bind(to_i64(version))
            .execute(&mut *conn)
            .await?;
        }
        None => {
            sqlx::query(
                "UPDATE provider_catalog_state
                    SET applied_version = 0, applied_sha256 = NULL, applied_at = NULL,
                        declined_version = MAX(declined_version, ?)
                  WHERE id = 1",
            )
            .bind(to_i64(version))
            .execute(&mut *conn)
            .await?;
        }
    }
    Ok(())
}

/// Replace the stored undo images of an applied catalog (used when a release
/// migration changed the rows under it and the catalog was re-applied).
pub async fn replace_undo(
    conn: &mut SqliteConnection,
    version: u64,
    undo_json: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE provider_catalog_history SET undo = ? WHERE version = ?")
        .bind(undo_json)
        .bind(to_i64(version))
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// A newer release has superseded every applied catalog: forget them. The
/// declined version is kept.
pub async fn clear_applied(conn: &mut SqliteConnection) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM provider_catalog_history")
        .execute(&mut *conn)
        .await?;
    sqlx::query(
        "UPDATE provider_catalog_state
            SET applied_version = 0, applied_sha256 = NULL, applied_at = NULL
          WHERE id = 1",
    )
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// The newest shared.db schema version applied.
pub async fn shared_schema_version(conn: &mut SqliteConnection) -> Result<u32, sqlx::Error> {
    let v: Option<i64> = sqlx::query_scalar("SELECT MAX(version) FROM schema_version")
        .fetch_one(&mut *conn)
        .await?;
    Ok(u32::try_from(v.unwrap_or(0)).unwrap_or(0))
}
