// src-tauri/src/persistence/persona_registry_store.rs
//
// The Persona registry in the user's encrypted integration_keys.db
// (keys_006.sql): display name, type and colour per Persona id.
//
// WHY HERE, NOT IN shared.db (items.id=661, decisions.id=862): shared.db is
// opened with no key so the login screen can read it. A Persona's name or
// type (for example a Medical or Legal Persona) is the domain information
// contextual separation exists to protect, so it must not be readable from
// disk without the password. shared.db keeps only personas(id, created_at) as
// the FK anchor for user_personas, focus_settings and the rest.
//
// PER USER, NOT PER PERSONA: a Persona can have several members
// (user_personas), so each user holds their own label for it. Persona sharing
// creates a separate materialized Persona for the recipient, which gets its
// own entry here when it is accepted. A second member added to an existing
// Persona would need an entry written from their own session; until then they
// see the placeholder below.
//
// CONNECTION STYLE: the `*_conn` functions take a connection the caller already
// holds (integration_keys_store::open_integration_keys_db migrates on every
// open, so callers open once and reuse). Runtime sqlx::query() only.

use std::collections::HashMap;

use sqlx::{Row, SqliteConnection};

use crate::persistence::integration_keys_store::{
    open_integration_keys_db, IntegrationKeysStoreError,
};

/// Shown for a Persona that has a shared.db row but no registry entry for this
/// user (for example a member added from another session). Never an error: the
/// list must still render.
pub const UNNAMED_PERSONA_NAME: &str = "Unnamed persona";
pub const UNNAMED_PERSONA_TYPE: &str = "standard";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersonaRegistryEntry {
    pub persona_id: String,
    pub display_name: String,
    pub persona_type: String,
    pub color: Option<String>,
    pub created_at: String,
}

/// Open the user's integration_keys.db (epoch-checked, migrated).
pub async fn open(
    user_id: &str,
    key_hex: &str,
) -> Result<SqliteConnection, IntegrationKeysStoreError> {
    open_integration_keys_db(user_id, key_hex).await
}

/// Insert a new entry. A second insert for the same Persona id fails on the
/// primary key, so an existing entry is never overwritten by a create.
pub async fn insert_conn(
    conn: &mut SqliteConnection,
    entry: &PersonaRegistryEntry,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO persona_registry
         (persona_id, display_name, persona_type, color, created_at)
         VALUES (?, ?, ?, ?, ?)",
    )
    .bind(&entry.persona_id)
    .bind(&entry.display_name)
    .bind(&entry.persona_type)
    .bind(&entry.color)
    .bind(&entry.created_at)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Insert or replace an entry. For a member writing their own label for a
/// Persona that already exists.
pub async fn upsert_conn(
    conn: &mut SqliteConnection,
    entry: &PersonaRegistryEntry,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT OR REPLACE INTO persona_registry
         (persona_id, display_name, persona_type, color, created_at)
         VALUES (?, ?, ?, ?, ?)",
    )
    .bind(&entry.persona_id)
    .bind(&entry.display_name)
    .bind(&entry.persona_type)
    .bind(&entry.color)
    .bind(&entry.created_at)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

fn row_to_entry(row: &sqlx::sqlite::SqliteRow) -> Result<PersonaRegistryEntry, sqlx::Error> {
    Ok(PersonaRegistryEntry {
        persona_id: row.try_get("persona_id")?,
        display_name: row.try_get("display_name")?,
        persona_type: row.try_get("persona_type")?,
        color: row.try_get("color")?,
        created_at: row.try_get("created_at")?,
    })
}

pub async fn get_conn(
    conn: &mut SqliteConnection,
    persona_id: &str,
) -> Result<Option<PersonaRegistryEntry>, sqlx::Error> {
    let row = sqlx::query(
        "SELECT persona_id, display_name, persona_type, color, created_at
         FROM persona_registry WHERE persona_id = ?",
    )
    .bind(persona_id)
    .fetch_optional(&mut *conn)
    .await?;
    row.as_ref().map(row_to_entry).transpose()
}

/// Every entry, keyed by Persona id. The registry holds one row per Persona
/// the user has, so reading all of it is cheaper than a variable-length IN.
pub async fn all_conn(
    conn: &mut SqliteConnection,
) -> Result<HashMap<String, PersonaRegistryEntry>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT persona_id, display_name, persona_type, color, created_at FROM persona_registry",
    )
    .fetch_all(&mut *conn)
    .await?;
    let mut map = HashMap::with_capacity(rows.len());
    for row in &rows {
        let entry = row_to_entry(row)?;
        map.insert(entry.persona_id.clone(), entry);
    }
    Ok(map)
}

/// Returns true if an entry was removed.
pub async fn delete_conn(
    conn: &mut SqliteConnection,
    persona_id: &str,
) -> Result<bool, sqlx::Error> {
    let result = sqlx::query("DELETE FROM persona_registry WHERE persona_id = ?")
        .bind(persona_id)
        .execute(&mut *conn)
        .await?;
    Ok(result.rows_affected() > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::SqliteConnectOptions;
    use sqlx::ConnectOptions;

    async fn conn() -> SqliteConnection {
        let mut conn = SqliteConnectOptions::new()
            .filename(":memory:")
            .connect()
            .await
            .expect("in-memory connection");
        crate::persistence::migrations::run_migrations(&mut conn, "keys", None)
            .await
            .expect("keys migrations must apply");
        conn
    }

    fn entry(id: &str, name: &str) -> PersonaRegistryEntry {
        PersonaRegistryEntry {
            persona_id: id.to_owned(),
            display_name: name.to_owned(),
            persona_type: "standard".to_owned(),
            color: Some("#112233".to_owned()),
            created_at: "2026-01-01T00:00:00Z".to_owned(),
        }
    }

    #[tokio::test]
    async fn insert_get_all_and_delete_round_trip() {
        let mut conn = conn().await;
        insert_conn(&mut conn, &entry("p1", "Alpha")).await.unwrap();
        insert_conn(&mut conn, &entry("p2", "Beta")).await.unwrap();

        assert_eq!(
            get_conn(&mut conn, "p1").await.unwrap(),
            Some(entry("p1", "Alpha"))
        );
        assert_eq!(get_conn(&mut conn, "nope").await.unwrap(), None);
        assert_eq!(all_conn(&mut conn).await.unwrap().len(), 2);

        assert!(delete_conn(&mut conn, "p1").await.unwrap());
        assert!(!delete_conn(&mut conn, "p1").await.unwrap());
        assert_eq!(all_conn(&mut conn).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn insert_never_overwrites_but_upsert_does() {
        let mut conn = conn().await;
        insert_conn(&mut conn, &entry("p1", "Alpha")).await.unwrap();
        insert_conn(&mut conn, &entry("p1", "Other"))
            .await
            .expect_err("a second insert for the same Persona must fail");
        assert_eq!(
            get_conn(&mut conn, "p1")
                .await
                .unwrap()
                .unwrap()
                .display_name,
            "Alpha"
        );

        upsert_conn(&mut conn, &entry("p1", "Renamed"))
            .await
            .unwrap();
        assert_eq!(
            get_conn(&mut conn, "p1")
                .await
                .unwrap()
                .unwrap()
                .display_name,
            "Renamed"
        );
    }

    #[tokio::test]
    async fn an_absent_colour_round_trips_as_none() {
        let mut conn = conn().await;
        let mut e = entry("p1", "Alpha");
        e.color = None;
        insert_conn(&mut conn, &e).await.unwrap();
        assert_eq!(
            get_conn(&mut conn, "p1").await.unwrap().unwrap().color,
            None
        );
    }
}
