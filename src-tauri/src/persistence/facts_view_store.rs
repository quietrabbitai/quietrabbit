// src-tauri/src/persistence/facts_view_store.rs
//
// items.id=959 unit 1 (decisions.id=968, 969): the READ side of the Library Facts
// view, over ONE Persona's personal.db. GATE-CRITICAL: the failure direction of
// this file is a read of another Persona's facts, or of a Focus-owned (device)
// entity.
//
// WHAT IS VISIBLE (and why each rule is a filter on the entity set, never on a
// name):
//   * a singleton fact (entity_id IS NULL) is the General row
//   * an entity is visible only when status = 'active' AND owner_focus IS NULL
//     (decisions.id=945: Focus-owned entities are not Facts) AND its parent is
//     visible too (decisions.id=416 invariant 14: a child inherits a stopped or
//     owned parent at READ time, nothing is written to the child)
//   * a fact is visible only when it is active (valid_until IS NULL) and belongs
//     to the General row or to a visible entity
//   * a fact of an invisible entity is not "hidden": its id resolves to nothing
//     (FactsViewError::FactNotFound), so a guessed id learns nothing
//
// ISOLATION. Nothing here takes a second Persona. The caller opens the one
// Persona's personal.db (personal_store::open_personal_db) and hands in that
// connection, so another Persona's rows are not in the file being read at all
// (decisions.id=546). The command layer checks the session user belongs to the
// Persona before opening it.
//
// FAILS CLOSED. Every error (an unreadable file, a wrong key, a malformed row)
// is one FactsViewError::Unreadable and no partial tree is built.
//
// NO ENTITY_STORE READER. entity_store's list/search take an EntityFilter, and its
// source scan counts the Unowned filter literals (three loaders). This module
// runs its own SQL, always with `owner_focus IS NULL` (a scan below pins it), and
// never touches search_entities (decisions.id=969 ruling 6).
//
// SEARCH. Names, aliases (json_each, invariant 12), labels and values, this
// Persona only. entity_facts.field_value is a BLOB column, so a value is compared
// as CAST(... AS TEXT) in SQL for aliases and as decoded text for the rest.

use std::collections::{HashMap, HashSet};

/// The stop marker (items.id=959 unit 2, decisions.id=969). A stop end-dates a fact
/// (valid_until = the stop timestamp) AND writes these two keys into the row's
/// extra_metadata (mutable; only the three provenance columns are immutable), so a
/// STOP is distinguishable from a SUPERSESSION. STOP_AT holds the timestamp, and
/// STOP_SCOPE says whether the user stopped the fact itself ("fact") or the section
/// it sat in ("section"), so use again on a section finds the facts ended with it
/// and leaves a fact that was stopped on its own alone. A stopped section carries
/// STOP_AT in its entities.extra_metadata (and status archived on the parent only),
/// which also tells a section WE stopped from one archived by something else.
/// A marked row is never an earlier value: use again writes the same value back, so
/// listing the stopped row as history would show it twice.
pub const STOP_AT: &str = "stopped_at";
pub const STOP_SCOPE: &str = "stop_scope";
pub const SCOPE_FACT: &str = "fact";
pub const SCOPE_SECTION: &str = "section";

/// The marker of a fact row's extra_metadata JSON text: (stopped_at, scope).
pub(crate) fn stop_marker(extra_metadata: &str) -> Option<(String, String)> {
    let v: serde_json::Value = serde_json::from_str(extra_metadata).ok()?;
    let at = v.get(STOP_AT)?.as_str()?.to_owned();
    let scope = v.get(STOP_SCOPE)?.as_str()?.to_owned();
    Some((at, scope))
}

use serde::{Deserialize, Serialize};
use specta::Type;
use sqlx::{Row, SqliteConnection};

/// What the view shows for where a fact came from (decisions.id=969 ruling 5).
/// A source value outside the mapping has NO provenance: the view never invents
/// a claim (the column default `interview`, and `synced_share`, show nothing).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum FactProvenance {
    /// user_confirmed, ipc, extract_confirm: the user entered or approved it.
    YouConfirmed,
    /// corrected.
    YouCorrected,
    /// conversation.
    FromChat,
    /// extracted.
    FoundByQuietRabbit,
}

/// Map `entity_facts.source` to its provenance, or None for any other value.
pub fn provenance_of(source: &str) -> Option<FactProvenance> {
    match source {
        "user_confirmed" | "ipc" | "extract_confirm" => Some(FactProvenance::YouConfirmed),
        "corrected" => Some(FactProvenance::YouCorrected),
        "conversation" => Some(FactProvenance::FromChat),
        "extracted" => Some(FactProvenance::FoundByQuietRabbit),
        _ => None,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct FactView {
    /// The fact row id, valid only inside this Persona.
    pub id: String,
    pub label: String,
    pub value: String,
    /// general, personal, medical or financial.
    pub sensitivity: String,
    pub provenance: Option<FactProvenance>,
    /// When this value began (valid_from, else created_at).
    pub since: String,
    /// How many earlier (end-dated) values the same label has.
    pub earlier_count: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct SectionView {
    pub id: String,
    pub entity_type: String,
    pub name: String,
    /// The parent section, or None for a top-level one.
    pub parent_id: Option<String>,
    pub facts: Vec<FactView>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct FactsTree {
    /// Singleton facts (entity_id IS NULL).
    pub general: Vec<FactView>,
    /// Top-level sections A to Z, each followed by its descendants A to Z.
    pub sections: Vec<SectionView>,
    /// Active facts shown, across General and every section.
    pub fact_count: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct EarlierValue {
    pub value: String,
    pub provenance: Option<FactProvenance>,
    pub since: String,
    /// When it stopped being the value (valid_until).
    pub until: String,
}

#[derive(Debug, thiserror::Error)]
pub enum FactsViewError {
    /// The Persona's file could not be opened or read. No partial result exists.
    #[error("facts_unreadable")]
    Unreadable,
    /// No visible fact has this id in this Persona (also: an owned entity's fact).
    #[error("fact_not_found")]
    FactNotFound,
    #[error("query_empty")]
    QueryEmpty,
    /// No visible section has this id in this Persona (also: an owned entity, a
    /// section that is already stopped, one archived by something else).
    #[error("section_not_found")]
    SectionNotFound,
    /// Use again refused: a newer active value for the same label exists.
    #[error("newer_value_exists")]
    NewerValueExists,
}

impl From<sqlx::Error> for FactsViewError {
    fn from(e: sqlx::Error) -> Self {
        log::warn!("facts_view_store: read failed: {e}");
        FactsViewError::Unreadable
    }
}

pub(crate) struct Ent {
    pub(crate) id: String,
    pub(crate) entity_type: String,
    pub(crate) name: String,
    pub(crate) parent_id: Option<String>,
}

struct Row_ {
    id: String,
    entity_id: Option<String>,
    label: String,
    value: String,
    sensitivity: String,
    source: String,
    since: String,
}

/// The visible sections, ordered root A to Z each followed by its descendants A to Z.
/// `owner_focus IS NULL` and `status = 'active'` are the two filters; a child
/// whose parent is not in that set is dropped (invariant 14, at read time).
pub(crate) async fn visible_entities(
    conn: &mut SqliteConnection,
) -> Result<Vec<Ent>, FactsViewError> {
    let rows = sqlx::query(
        "SELECT id, entity_type, display_name, aliases, parent_entity_id
         FROM entities
         WHERE owner_focus IS NULL AND status = 'active'
         ORDER BY lower(display_name), id",
    )
    .fetch_all(&mut *conn)
    .await?;

    let mut all: Vec<Ent> = Vec::with_capacity(rows.len());
    for r in &rows {
        let aliases_json: String = r.try_get("aliases")?;
        // Not kept (search reads aliases with json_each), but a malformed value
        // still fails the whole read closed.
        serde_json::from_str::<Vec<String>>(&aliases_json)
            .map_err(|_| FactsViewError::Unreadable)?;
        all.push(Ent {
            id: r.try_get("id")?,
            entity_type: r.try_get("entity_type")?,
            name: r.try_get("display_name")?,
            parent_id: r.try_get("parent_entity_id")?,
        });
    }

    // Visible = an active, unowned chain up to a root. A parent that is stopped,
    // owned or absent is not in `all`, so its whole subtree falls out.
    let by_id: HashMap<&str, &Ent> = all.iter().map(|e| (e.id.as_str(), e)).collect();
    let mut keep: HashSet<String> = HashSet::new();
    for e in &all {
        let mut cur = e;
        let mut seen: HashSet<&str> = HashSet::new();
        let ok = loop {
            if !seen.insert(cur.id.as_str()) {
                break false; // a parent cycle is never shown
            }
            match &cur.parent_id {
                None => break true,
                Some(p) => match by_id.get(p.as_str()) {
                    Some(parent) => cur = parent,
                    None => break false,
                },
            }
        };
        if ok {
            keep.insert(e.id.clone());
        }
    }
    let kept: Vec<Ent> = all.into_iter().filter(|e| keep.contains(&e.id)).collect();

    // Order: each root, then its descendants depth first, both A to Z (kept is
    // already sorted by name).
    let mut children: HashMap<String, Vec<usize>> = HashMap::new();
    let mut roots: Vec<usize> = Vec::new();
    for (i, e) in kept.iter().enumerate() {
        match &e.parent_id {
            None => roots.push(i),
            Some(p) => children.entry(p.clone()).or_default().push(i),
        }
    }
    let mut order: Vec<usize> = Vec::with_capacity(kept.len());
    let mut stack: Vec<usize> = roots.into_iter().rev().collect();
    while let Some(i) = stack.pop() {
        order.push(i);
        if let Some(kids) = children.get(&kept[i].id) {
            for k in kids.iter().rev() {
                stack.push(*k);
            }
        }
    }
    let mut slots: Vec<Option<Ent>> = kept.into_iter().map(Some).collect();
    Ok(order.into_iter().filter_map(|i| slots[i].take()).collect())
}

/// Active facts of the General row and of the given visible entities, plus the
/// count of earlier values per (entity, label).
async fn active_rows(
    conn: &mut SqliteConnection,
    visible: &HashSet<&str>,
) -> Result<(Vec<Row_>, HashMap<(Option<String>, String), u32>), FactsViewError> {
    let rows = sqlx::query(
        "SELECT id, entity_id, field_name, field_value, sensitivity, source,
                COALESCE(valid_from, created_at) AS since, valid_until, extra_metadata
         FROM entity_facts
         ORDER BY lower(field_name), created_at, id",
    )
    .fetch_all(&mut *conn)
    .await?;

    let mut active = Vec::new();
    let mut earlier: HashMap<(Option<String>, String), u32> = HashMap::new();
    for r in &rows {
        let entity_id: Option<String> = r.try_get("entity_id")?;
        if let Some(eid) = &entity_id {
            if !visible.contains(eid.as_str()) {
                continue;
            }
        }
        let label: String = r.try_get("field_name")?;
        let valid_until: Option<String> = r.try_get("valid_until")?;
        if valid_until.is_some() {
            // A stopped row is removed, never an earlier value (see STOP_AT).
            let meta: String = r.try_get("extra_metadata")?;
            if stop_marker(&meta).is_some() {
                continue;
            }
            *earlier.entry((entity_id, label)).or_insert(0) += 1;
            continue;
        }
        active.push(Row_ {
            id: r.try_get("id")?,
            entity_id,
            label,
            value: r.try_get("field_value")?,
            sensitivity: r.try_get("sensitivity")?,
            source: r.try_get("source")?,
            since: r.try_get("since")?,
        });
    }
    Ok((active, earlier))
}

fn fact_view(r: &Row_, earlier: &HashMap<(Option<String>, String), u32>) -> FactView {
    FactView {
        id: r.id.clone(),
        label: r.label.clone(),
        value: r.value.clone(),
        sensitivity: r.sensitivity.clone(),
        provenance: provenance_of(&r.source),
        since: r.since.clone(),
        earlier_count: earlier
            .get(&(r.entity_id.clone(), r.label.clone()))
            .copied()
            .unwrap_or(0),
    }
}

async fn load_tree(
    conn: &mut SqliteConnection,
) -> Result<(FactsTree, Vec<Ent>, Vec<Row_>), FactsViewError> {
    let ents = visible_entities(conn).await?;
    let ids: HashSet<&str> = ents.iter().map(|e| e.id.as_str()).collect();
    let (rows, earlier) = active_rows(conn, &ids).await?;

    let general: Vec<FactView> = rows
        .iter()
        .filter(|r| r.entity_id.is_none())
        .map(|r| fact_view(r, &earlier))
        .collect();
    let sections: Vec<SectionView> = ents
        .iter()
        .map(|e| SectionView {
            id: e.id.clone(),
            entity_type: e.entity_type.clone(),
            name: e.name.clone(),
            parent_id: e.parent_id.clone(),
            facts: rows
                .iter()
                .filter(|r| r.entity_id.as_deref() == Some(e.id.as_str()))
                .map(|r| fact_view(r, &earlier))
                .collect(),
        })
        .collect();
    let fact_count =
        u32::try_from(general.len() + sections.iter().map(|s| s.facts.len()).sum::<usize>())
            .unwrap_or(u32::MAX);
    Ok((
        FactsTree {
            general,
            sections,
            fact_count,
        },
        ents,
        rows,
    ))
}

/// The Persona's whole Summary: General, then the visible sections.
pub async fn facts_tree_conn(conn: &mut SqliteConnection) -> Result<FactsTree, FactsViewError> {
    Ok(load_tree(conn).await?.0)
}

/// Earlier values of one visible fact's label, newest first. A fact id that is not
/// a visible fact of this Persona (another Persona's, an owned entity's, a stopped
/// section's, an end-dated row's, or no row) is FactNotFound.
pub async fn fact_history_conn(
    conn: &mut SqliteConnection,
    fact_id: &str,
) -> Result<Vec<EarlierValue>, FactsViewError> {
    let ents = visible_entities(conn).await?;
    let ids: HashSet<&str> = ents.iter().map(|e| e.id.as_str()).collect();
    let (rows, _) = active_rows(conn, &ids).await?;
    let fact = rows
        .iter()
        .find(|r| r.id == fact_id)
        .ok_or(FactsViewError::FactNotFound)?;

    let hist = sqlx::query(
        "SELECT field_value, source, COALESCE(valid_from, created_at) AS since, valid_until
         FROM entity_facts
         WHERE field_name = ? AND valid_until IS NOT NULL
           AND json_extract(extra_metadata, '$.stopped_at') IS NULL
           AND ((entity_id IS NULL AND ? IS NULL) OR entity_id = ?)
         ORDER BY valid_until DESC, created_at DESC, id",
    )
    .bind(&fact.label)
    .bind(&fact.entity_id)
    .bind(&fact.entity_id)
    .fetch_all(&mut *conn)
    .await?;
    let mut out = Vec::with_capacity(hist.len());
    for r in &hist {
        let source: String = r.try_get("source")?;
        out.push(EarlierValue {
            value: r.try_get("field_value")?,
            provenance: provenance_of(&source),
            since: r.try_get("since")?,
            until: r.try_get("valid_until")?,
        });
    }
    Ok(out)
}

fn matches(hay: &str, needle: &str) -> bool {
    hay.to_lowercase().contains(needle)
}

/// Search this Persona only, over names, aliases (json_each), labels and values.
/// Shows only the matching facts, except that a section matched by its name or an
/// alias shows all its facts. Sections with nothing to show are dropped.
pub async fn search_facts_conn(
    conn: &mut SqliteConnection,
    query: &str,
) -> Result<FactsTree, FactsViewError> {
    let needle = query.trim().to_lowercase();
    if needle.is_empty() {
        return Err(FactsViewError::QueryEmpty);
    }
    let (tree, _, _) = load_tree(conn).await?;

    // Aliases are searched with json_each (invariant 12), then intersected with
    // the visible set, so an owned or stopped entity's alias never matches.
    let pattern = format!("%{}%", escape_like(&needle));
    let alias_rows = sqlx::query(
        "SELECT id FROM entities
         WHERE owner_focus IS NULL AND status = 'active'
           AND EXISTS (SELECT 1 FROM json_each(entities.aliases)
                       WHERE lower(json_each.value) LIKE ? ESCAPE '\\')",
    )
    .bind(&pattern)
    .fetch_all(&mut *conn)
    .await?;
    let mut alias_hit: HashSet<String> = HashSet::new();
    for r in &alias_rows {
        alias_hit.insert(r.try_get("id")?);
    }

    let fact_hit = |f: &FactView| matches(&f.label, &needle) || matches(&f.value, &needle);
    let general: Vec<FactView> = tree.general.into_iter().filter(|f| fact_hit(f)).collect();
    let parent_of: HashMap<String, Option<String>> = tree
        .sections
        .iter()
        .map(|s| (s.id.clone(), s.parent_id.clone()))
        .collect();

    // A section is a hit when its name or an alias matches (all its facts show) or
    // any of its facts match (only those show).
    let mut shown: Vec<SectionView> = Vec::new();
    for s in tree.sections {
        let by_name = matches(&s.name, &needle) || alias_hit.contains(&s.id);
        let facts: Vec<FactView> = if by_name {
            s.facts.clone()
        } else {
            s.facts.iter().filter(|f| fact_hit(f)).cloned().collect()
        };
        if by_name || !facts.is_empty() {
            shown.push(SectionView { facts, ..s });
        }
    }
    // Keep a hit reachable: pull in each ancestor, with no facts of its own shown.
    let mut want: HashSet<String> = shown.iter().map(|s| s.id.clone()).collect();
    for s in &shown {
        let mut cur = parent_of.get(&s.id).cloned().flatten();
        while let Some(p) = cur {
            if !want.insert(p.clone()) {
                break;
            }
            cur = parent_of.get(&p).cloned().flatten();
        }
    }
    let hits: HashMap<String, SectionView> = shown.into_iter().map(|s| (s.id.clone(), s)).collect();
    let (full, _, _) = load_tree(conn).await?;
    let mut sections = Vec::new();
    let mut count = general.len();
    for s in full.sections {
        if let Some(h) = hits.get(&s.id) {
            count += h.facts.len();
            sections.push(h.clone());
        } else if want.contains(&s.id) {
            sections.push(SectionView { facts: vec![], ..s });
        }
    }
    Ok(FactsTree {
        general,
        sections,
        fact_count: u32::try_from(count).unwrap_or(u32::MAX),
    })
}

fn escape_like(term: &str) -> String {
    let mut out = String::with_capacity(term.len());
    for ch in term.chars() {
        if ch == '%' || ch == '_' || ch == '\\' {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct RemovedFact {
    /// The stopped row's id: what use again takes.
    pub id: String,
    /// None = the General row.
    pub section_id: Option<String>,
    pub section_name: Option<String>,
    pub label: String,
    pub value: String,
    pub sensitivity: String,
    pub stopped_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct RemovedSection {
    pub id: String,
    pub entity_type: String,
    pub name: String,
    pub parent_id: Option<String>,
    /// Facts that were stopped together with the section (what use again restores).
    pub fact_count: u32,
    pub stopped_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct RemovedList {
    pub facts: Vec<RemovedFact>,
    pub sections: Vec<RemovedSection>,
    /// facts plus sections: the Removed (n) count.
    pub count: u32,
}

/// What the user has stopped, in this Persona. A removed FACT is a (section, label)
/// with no active row whose latest ended row carries a fact-scope stop marker, in the
/// General row or a visible section (a fact inside a stopped section is listed with
/// that section, not alone). A removed SECTION is an unowned archived entity that WE
/// stopped (it carries the marker, so an entity archived by something else is not
/// listed) and whose parent is visible.
pub async fn list_removed_conn(conn: &mut SqliteConnection) -> Result<RemovedList, FactsViewError> {
    let ents = visible_entities(conn).await?;
    let names: HashMap<&str, &str> = ents
        .iter()
        .map(|e| (e.id.as_str(), e.name.as_str()))
        .collect();

    let rows = sqlx::query(
        "SELECT id, entity_id, field_name, field_value, sensitivity, valid_until,
                created_at, extra_metadata
         FROM entity_facts",
    )
    .fetch_all(&mut *conn)
    .await?;

    struct R {
        id: String,
        value: String,
        sensitivity: String,
        ended: Option<(String, String)>, // (valid_until, created_at)
        marker: Option<(String, String)>,
    }
    let mut by_key: HashMap<(Option<String>, String), Vec<R>> = HashMap::new();
    for r in &rows {
        let meta: String = r.try_get("extra_metadata")?;
        let valid_until: Option<String> = r.try_get("valid_until")?;
        let created: String = r.try_get("created_at")?;
        by_key
            .entry((r.try_get("entity_id")?, r.try_get("field_name")?))
            .or_default()
            .push(R {
                id: r.try_get("id")?,
                value: r.try_get("field_value")?,
                sensitivity: r.try_get("sensitivity")?,
                ended: valid_until.map(|u| (u, created)),
                marker: stop_marker(&meta),
            });
    }

    let mut facts: Vec<RemovedFact> = Vec::new();
    let mut section_fact_count: HashMap<(String, String), u32> = HashMap::new();
    for ((entity_id, label), group) in &by_key {
        // Facts ended together with a section, by (section, stamp).
        if let Some(eid) = entity_id {
            for r in group {
                if let Some((at, scope)) = &r.marker {
                    if scope == SCOPE_SECTION {
                        *section_fact_count
                            .entry((eid.clone(), at.clone()))
                            .or_insert(0) += 1;
                    }
                }
            }
        }
        if let Some(eid) = entity_id {
            if !names.contains_key(eid.as_str()) {
                continue;
            }
        }
        if group.iter().any(|r| r.ended.is_none()) {
            continue; // a value is in use
        }
        let Some(latest) = group.iter().max_by(|a, b| a.ended.cmp(&b.ended)) else {
            continue;
        };
        if let Some((at, scope)) = &latest.marker {
            if scope == SCOPE_FACT {
                facts.push(RemovedFact {
                    id: latest.id.clone(),
                    section_id: entity_id.clone(),
                    section_name: entity_id
                        .as_deref()
                        .and_then(|e| names.get(e).map(|n| (*n).to_owned())),
                    label: label.clone(),
                    value: latest.value.clone(),
                    sensitivity: latest.sensitivity.clone(),
                    stopped_at: at.clone(),
                });
            }
        }
    }
    facts.sort_by(|a, b| {
        (a.section_name.as_deref(), a.label.to_lowercase(), &a.id).cmp(&(
            b.section_name.as_deref(),
            b.label.to_lowercase(),
            &b.id,
        ))
    });

    let visible: HashSet<&str> = names.keys().copied().collect();
    let srows = sqlx::query(
        "SELECT id, entity_type, display_name, parent_entity_id,
                json_extract(extra_metadata, '$.stopped_at') AS stopped_at
         FROM entities
         WHERE owner_focus IS NULL AND status = 'archived'
           AND json_extract(extra_metadata, '$.stopped_at') IS NOT NULL
         ORDER BY lower(display_name), id",
    )
    .fetch_all(&mut *conn)
    .await?;
    let mut sections = Vec::new();
    for r in &srows {
        let parent_id: Option<String> = r.try_get("parent_entity_id")?;
        if let Some(p) = &parent_id {
            if !visible.contains(p.as_str()) {
                continue; // it sits under a stopped or owned parent
            }
        }
        let id: String = r.try_get("id")?;
        let stopped_at: String = r.try_get("stopped_at")?;
        let fact_count = section_fact_count
            .get(&(id.clone(), stopped_at.clone()))
            .copied()
            .unwrap_or(0);
        sections.push(RemovedSection {
            id,
            entity_type: r.try_get("entity_type")?,
            name: r.try_get("display_name")?,
            parent_id,
            fact_count,
            stopped_at,
        });
    }
    let count = u32::try_from(facts.len() + sections.len()).unwrap_or(u32::MAX);
    Ok(RemovedList {
        facts,
        sections,
        count,
    })
}
