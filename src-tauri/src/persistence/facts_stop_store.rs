// src-tauri/src/persistence/facts_stop_store.rs
//
// items.id=959 unit 2 (decisions.id=968, 969): the WRITE side of the Library Facts
// view, over ONE Persona's personal.db. GATE-CRITICAL: the failure directions are a
// write to another Persona's facts, a write to a Focus-owned (device) entity, and a
// removal that cannot be undone.
//
// Removal is END-DATING ONLY (decisions.id=968): nothing here deletes a fact or an
// entity, and a scan in my_facts.rs pins that no DELETE statement exists in this
// file. Stop using marks (valid_until plus the stop marker, see facts_view_store);
// use again starts a NEW active row (decisions.id=416 invariant 8), so history is
// kept. A stopped section sets status 'archived' on the section ITSELF only, never
// on a descendant (invariant 14). But the context loader and the share and sync
// loaders do not read the parent chain, so the stop ALSO end-dates the active facts
// of every descendant section, under the same timestamp and section marker: that is
// what stops them being read. A child stopped on its own has its own timestamp, so
// it stays stopped when its parent comes back.
//
// WHICH ROWS A WRITE MAY TOUCH. Every write first resolves its id inside the
// visible set that the read side uses (active, unowned, parent chain visible), so
// an owned entity, another Persona's id (that file is not even open), a section
// already stopped and a section archived by something else all resolve to nothing
// (not found) and nothing is written. Use again resolves a stopped row by the same
// rule against its entity. Multi-statement writes run in one SAVEPOINT.
//
// NO COLUMN IS ADDED: the stop marker lives in extra_metadata (mutable), the three
// provenance columns are never touched (use again COPIES them onto the new row, so a
// fact copied in from another Persona stays marked as such, decisions.id=546).

use std::collections::HashSet;

use sqlx::{Row, SqliteConnection};

use crate::persistence::entity_store::{retire_entity_conn, update_entity_conn, EntityUpdate};
use crate::persistence::facts_view_store::{
    stop_marker, visible_entities, FactsViewError, SCOPE_FACT, STOP_AT, STOP_SCOPE,
};
use crate::persistence::personal_store::create_entity_fact_with_policy_conn;
use crate::persistence::savepoint::rollback_to_and_release;
use crate::persistence::source_registry_store::mark_record_user_modified_conn;

fn db<E: std::fmt::Display>(e: E) -> FactsViewError {
    log::warn!("facts_stop_store: write failed: {e}");
    FactsViewError::Unreadable
}

async fn visible_ids(conn: &mut SqliteConnection) -> Result<HashSet<String>, FactsViewError> {
    Ok(visible_entities(conn)
        .await?
        .into_iter()
        .map(|e| e.id)
        .collect())
}

/// Run `f` in a SAVEPOINT: released on Ok, rolled back and released on Err.
macro_rules! atomic {
    ($conn:expr, $name:literal, $body:expr) => {{
        sqlx::query(concat!("SAVEPOINT ", $name))
            .execute(&mut *$conn)
            .await
            .map_err(db)?;
        let step: Result<_, FactsViewError> = $body;
        match step {
            Ok(v) => {
                sqlx::query(concat!("RELEASE ", $name))
                    .execute(&mut *$conn)
                    .await
                    .map_err(db)?;
                Ok(v)
            }
            Err(e) => {
                let _ = rollback_to_and_release($conn, $name).await;
                Err(e)
            }
        }
    }};
}

/// Stop using one visible, active fact: end-date it and mark it a fact-scope stop.
pub async fn stop_fact_conn(
    conn: &mut SqliteConnection,
    fact_id: &str,
) -> Result<(), FactsViewError> {
    let visible = visible_ids(conn).await?;
    let row =
        sqlx::query("SELECT entity_id FROM entity_facts WHERE id = ? AND valid_until IS NULL")
            .bind(fact_id)
            .fetch_optional(&mut *conn)
            .await
            .map_err(db)?
            .ok_or(FactsViewError::FactNotFound)?;
    let entity_id: Option<String> = row.try_get("entity_id").map_err(db)?;
    if let Some(eid) = &entity_id {
        if !visible.contains(eid) {
            return Err(FactsViewError::FactNotFound);
        }
    }
    let ts = crate::providers::utils::now();
    atomic!(conn, "facts_stop_fact", {
        async {
            let n = sqlx::query(
                "UPDATE entity_facts
                 SET valid_until = ?,
                     extra_metadata = json_set(CASE WHEN json_type(extra_metadata) = 'object' THEN extra_metadata ELSE '{}' END,
                                               '$.stopped_at', ?, '$.stop_scope', 'fact')
                 WHERE id = ? AND valid_until IS NULL",
            )
            .bind(&ts)
            .bind(&ts)
            .bind(fact_id)
            .execute(&mut *conn)
            .await
            .map_err(db)?
            .rows_affected();
            if n != 1 {
                return Err(FactsViewError::FactNotFound);
            }
            if let Some(eid) = &entity_id {
                mark_record_user_modified_conn(conn, eid)
                    .await
                    .map_err(db)?;
            }
            Ok(())
        }
        .await
    })
}

/// Stop using a visible section: status archived on the section ONLY, the stop
/// marker on that entity, and the active facts of the section AND of every
/// descendant section end-dated with one shared timestamp and the section marker.
/// No descendant's status or metadata is written. A descendant whose facts are
/// ended is flipped pristine -> user_modified (as the section is), so a persona
/// sync holds it for review instead of writing its facts back. Focus-owned
/// entities are never walked into. Returns how many facts were ended.
pub async fn stop_section_conn(
    conn: &mut SqliteConnection,
    section_id: &str,
) -> Result<u32, FactsViewError> {
    if !visible_ids(conn).await?.contains(section_id) {
        return Err(FactsViewError::SectionNotFound);
    }
    let ts = crate::providers::utils::now();
    atomic!(conn, "facts_stop_section", {
        async {
            retire_entity_conn(conn, section_id, "archived")
                .await
                .map_err(db)?;
            let n = sqlx::query(
                "UPDATE entities
                 SET extra_metadata = json_set(
                         CASE WHEN json_type(extra_metadata) = 'object'
                              THEN extra_metadata ELSE '{}' END,
                         '$.stopped_at', ?)
                 WHERE id = ? AND owner_focus IS NULL",
            )
            .bind(&ts)
            .bind(section_id)
            .execute(&mut *conn)
            .await
            .map_err(db)?
            .rows_affected();
            if n != 1 {
                return Err(FactsViewError::SectionNotFound);
            }
            // The section and its descendants that have something to end.
            let touched: Vec<String> = sqlx::query_scalar(&format!(
                "{SUBTREE}
                 SELECT DISTINCT entity_id FROM entity_facts
                 WHERE valid_until IS NULL AND entity_id IN (SELECT id FROM subtree)"
            ))
            .bind(section_id)
            .fetch_all(&mut *conn)
            .await
            .map_err(db)?;
            let ended = sqlx::query(&format!(
                "{SUBTREE}
                 UPDATE entity_facts
                 SET valid_until = ?,
                     extra_metadata = json_set(CASE WHEN json_type(extra_metadata) = 'object' THEN extra_metadata ELSE '{{}}' END,
                                               '$.stopped_at', ?, '$.stop_scope', 'section')
                 WHERE valid_until IS NULL AND entity_id IN (SELECT id FROM subtree)"
            ))
            .bind(section_id)
            .bind(&ts)
            .bind(&ts)
            .execute(&mut *conn)
            .await
            .map_err(db)?
            .rows_affected();
            mark_record_user_modified_conn(conn, section_id)
                .await
                .map_err(db)?;
            for id in touched.iter().filter(|id| id.as_str() != section_id) {
                mark_record_user_modified_conn(conn, id)
                    .await
                    .map_err(db)?;
            }
            Ok(u32::try_from(ended).unwrap_or(u32::MAX))
        }
        .await
    })
}

/// The section bound first and every unowned entity under it, whatever its status
/// (UNION, so a parent cycle ends).
const SUBTREE: &str = "WITH RECURSIVE subtree(id) AS (
    SELECT ?
    UNION
    SELECT e.id FROM entities e JOIN subtree s ON e.parent_entity_id = s.id
    WHERE e.owner_focus IS NULL)";

struct Stopped {
    id: String,
    entity_id: Option<String>,
    label: String,
    value: String,
    sensitivity: String,
    tier2: String,
    tier3: String,
    source: String,
    source_persona_id: String,
    cross: bool,
    origin: Option<String>,
    metadata: serde_json::Value,
}

fn stopped_from(r: &sqlx::sqlite::SqliteRow) -> Result<Stopped, FactsViewError> {
    let meta: String = r.try_get("extra_metadata").map_err(db)?;
    let mut metadata: serde_json::Value = serde_json::from_str(&meta).map_err(db)?;
    if let Some(o) = metadata.as_object_mut() {
        // The new row is a live value: it must not carry the old row's stop marker.
        o.remove(STOP_AT);
        o.remove(STOP_SCOPE);
    }
    let cross: i64 = r.try_get("cross_persona_export").map_err(db)?;
    Ok(Stopped {
        id: r.try_get("id").map_err(db)?,
        entity_id: r.try_get("entity_id").map_err(db)?,
        label: r.try_get("field_name").map_err(db)?,
        value: r.try_get("field_value").map_err(db)?,
        sensitivity: r.try_get("sensitivity").map_err(db)?,
        tier2: r.try_get("abstraction_tier2").map_err(db)?,
        tier3: r.try_get("abstraction_tier3").map_err(db)?,
        source: r.try_get("source").map_err(db)?,
        source_persona_id: r.try_get("source_persona_id").map_err(db)?,
        cross: cross != 0,
        origin: r.try_get("origin_persona_id").map_err(db)?,
        metadata,
    })
}

const STOPPED_COLUMNS: &str = "id, entity_id, field_name, field_value, sensitivity, \
    abstraction_tier2, abstraction_tier3, source, source_persona_id, \
    cross_persona_export, origin_persona_id, extra_metadata";

/// Is a value in use for this (entity, label)?
async fn active_exists(
    conn: &mut SqliteConnection,
    entity_id: Option<&str>,
    label: &str,
) -> Result<bool, FactsViewError> {
    let n: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM entity_facts
         WHERE field_name = ? AND valid_until IS NULL
           AND ((entity_id IS NULL AND ? IS NULL) OR entity_id = ?)",
    )
    .bind(label)
    .bind(entity_id)
    .bind(entity_id)
    .fetch_one(&mut *conn)
    .await
    .map_err(db)?;
    Ok(n > 0)
}

/// Start a NEW active row with the stopped row's value, from now (invariant 8).
async fn restore_row(conn: &mut SqliteConnection, s: &Stopped) -> Result<(), FactsViewError> {
    create_entity_fact_with_policy_conn(
        conn,
        s.entity_id.as_deref(),
        &s.label,
        &s.value,
        &s.sensitivity,
        &s.tier2,
        &s.tier3,
        &s.source,
        &s.source_persona_id,
        s.cross,
        s.origin.as_deref(),
        Some(s.metadata.clone()),
    )
    .await
    .map_err(db)?;
    Ok(())
}

/// Use again a fact the user stopped on its own. `stopped_id` is the stopped row's id
/// (RemovedFact.id). Refused with NewerValueExists when a value for the same label is
/// already in use (the unique-active indexes), and FactNotFound when the row is not a
/// fact-scope stop, is not the label's latest ended row (already used again), or sits
/// in an owned, stopped or other-Persona section.
pub async fn use_again_fact_conn(
    conn: &mut SqliteConnection,
    stopped_id: &str,
) -> Result<(), FactsViewError> {
    let visible = visible_ids(conn).await?;
    let row = sqlx::query(&format!(
        "SELECT {STOPPED_COLUMNS}, valid_until FROM entity_facts WHERE id = ?"
    ))
    .bind(stopped_id)
    .fetch_optional(&mut *conn)
    .await
    .map_err(db)?
    .ok_or(FactsViewError::FactNotFound)?;
    let valid_until: Option<String> = row.try_get("valid_until").map_err(db)?;
    let meta: String = row.try_get("extra_metadata").map_err(db)?;
    let s = stopped_from(&row)?;
    let is_fact_stop =
        valid_until.is_some() && stop_marker(&meta).is_some_and(|(_, scope)| scope == SCOPE_FACT);
    if !is_fact_stop {
        return Err(FactsViewError::FactNotFound);
    }
    if let Some(eid) = &s.entity_id {
        if !visible.contains(eid) {
            return Err(FactsViewError::FactNotFound);
        }
    }
    if active_exists(conn, s.entity_id.as_deref(), &s.label).await? {
        return Err(FactsViewError::NewerValueExists);
    }
    // Only the label's latest ended row may come back: an older stop was already
    // used again (and since stopped again, or superseded).
    let later: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM entity_facts
         WHERE field_name = ? AND id <> ? AND valid_until IS NOT NULL AND valid_until > ?
           AND ((entity_id IS NULL AND ? IS NULL) OR entity_id = ?)",
    )
    .bind(&s.label)
    .bind(&s.id)
    .bind(valid_until.as_deref())
    .bind(s.entity_id.as_deref())
    .bind(s.entity_id.as_deref())
    .fetch_one(&mut *conn)
    .await
    .map_err(db)?;
    if later > 0 {
        return Err(FactsViewError::FactNotFound);
    }
    atomic!(conn, "facts_use_again_fact", {
        async {
            restore_row(conn, &s).await?;
            if let Some(eid) = &s.entity_id {
                mark_record_user_modified_conn(conn, eid)
                    .await
                    .map_err(db)?;
            }
            Ok(())
        }
        .await
    })
}

/// Use again a section WE stopped: status back to active, the stop marker removed,
/// and a new active row for each fact ended at the section's shared timestamp, in
/// the section and in the descendants stopped with it (a fact or a child section
/// stopped on its own earlier stays stopped). The facts are found by the shared
/// timestamp, not by today's parent chain, so one moved since still comes back. Atomic: a conflict (a value for
/// one of those labels already in use) refuses the whole restore with
/// NewerValueExists and writes nothing. Returns how many facts came back.
pub async fn use_again_section_conn(
    conn: &mut SqliteConnection,
    section_id: &str,
) -> Result<u32, FactsViewError> {
    let row = sqlx::query(
        "SELECT json_extract(extra_metadata, '$.stopped_at') AS stopped_at
         FROM entities
         WHERE id = ? AND owner_focus IS NULL AND status = 'archived'
           AND json_extract(extra_metadata, '$.stopped_at') IS NOT NULL",
    )
    .bind(section_id)
    .fetch_optional(&mut *conn)
    .await
    .map_err(db)?
    .ok_or(FactsViewError::SectionNotFound)?;
    let stopped_at: String = row.try_get("stopped_at").map_err(db)?;

    let rows = sqlx::query(&format!(
        "SELECT {STOPPED_COLUMNS} FROM entity_facts
         WHERE valid_until = ?
           AND json_extract(extra_metadata, '$.stop_scope') = 'section'
           AND json_extract(extra_metadata, '$.stopped_at') = ?
           AND entity_id IN (SELECT id FROM entities WHERE owner_focus IS NULL)
         ORDER BY entity_id, field_name, id"
    ))
    .bind(&stopped_at)
    .bind(&stopped_at)
    .fetch_all(&mut *conn)
    .await
    .map_err(db)?;
    let mut facts = Vec::with_capacity(rows.len());
    for r in &rows {
        facts.push(stopped_from(r)?);
    }
    for s in &facts {
        if active_exists(conn, s.entity_id.as_deref(), &s.label).await? {
            return Err(FactsViewError::NewerValueExists);
        }
    }

    atomic!(conn, "facts_use_again_section", {
        async {
            for s in &facts {
                restore_row(conn, s).await?;
            }
            update_entity_conn(
                conn,
                section_id,
                &EntityUpdate {
                    status: Some("active".to_owned()),
                    ..Default::default()
                },
            )
            .await
            .map_err(db)?;
            let n = sqlx::query(
                "UPDATE entities
                 SET extra_metadata = json_remove(extra_metadata, '$.stopped_at')
                 WHERE id = ? AND owner_focus IS NULL",
            )
            .bind(section_id)
            .execute(&mut *conn)
            .await
            .map_err(db)?
            .rows_affected();
            if n != 1 {
                return Err(FactsViewError::SectionNotFound);
            }
            let mut touched: HashSet<&str> = facts
                .iter()
                .filter_map(|f| f.entity_id.as_deref())
                .collect();
            touched.insert(section_id);
            for id in touched {
                mark_record_user_modified_conn(conn, id).await.map_err(db)?;
            }
            Ok(u32::try_from(facts.len()).unwrap_or(u32::MAX))
        }
        .await
    })
}
