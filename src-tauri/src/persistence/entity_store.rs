// src-tauri/src/persistence/entity_store.rs
//
// cb-01 — Entity-scoped structured record store + search.
// Foundation block, items.id=128 / decisions.id=621 §1 (catalog_status
// 'confirmed', 5-6 named adopters: Cooking recipes, Travel trip/companion
// entities, Writing Assistant document library, Class deliverables/topics,
// Research & Purchase item lines/candidates).
//
// Operates on the `entities` table in personal.db — per-user, per-persona,
// SQLCipher encrypted. Path and key handling are NOT duplicated here: this
// module reuses personal_store::open_personal_db and PersonalStoreError,
// because it is the same physical database file. A second opener would be a
// P4 (One Home) violation.
//
// RELATIONSHIP TO entity_facts:
//   entities  — the record itself (identity, type, display name, aliases,
//               hierarchy, lifecycle status). Owned by this module.
//   entity_facts — facts ABOUT an entity, one row per (entity, field_name).
//               Owned by personal_store.rs (built under items.id=27).
//   The two are complementary halves; neither supersedes the other.
//
// DELIBERATELY NOT BUILT — both are recorded scope decisions, not omissions:
//
//   1. Relationship-tracking (new/update/fork/reference/supersede).
//      cb-01 lists this as an OPTIONAL write-side capability, but its
//      backing table `entity_relationships` is declared in personal_001.sql
//      as "stub — R1: no reads, no writes, no IPC. Reserved for post-R1
//      relationship modelling." The schema's own R1 declaration governs.
//
//   2. Hard delete.
//      Not named in cb-01's description, and actively hazardous here:
//      PRAGMA foreign_keys is not set anywhere in this codebase (see the
//      note in outputs_001.sql), so `entity_facts.entity_id ... ON DELETE
//      CASCADE` does NOT fire at runtime. A DELETE FROM entities would
//      silently orphan immutable entity_facts provenance rows rather than
//      cascading. retire_entity() (a status transition) plus
//      check_entity_cascade() (read-only impact report) cover the stated
//      capability without a destructive path. If hard delete is ever added,
//      it must delete dependent facts explicitly inside a SAVEPOINT and
//      must not rely on the unfired cascade.
//
// TESTABILITY: every public function opens the DB and delegates to a
// `*_conn(&mut SqliteConnection, ...)` inner function — mirroring the
// existing resolve_voice_profile_conn pattern in personal_store.rs. Tests
// exercise the _conn layer against an in-memory SQLite database seeded from
// schema/personal_001.sql, so the block has real DB-backed coverage without
// a QR_DATA_ROOT or SQLCipher key.
//
// QUERY STYLE: runtime sqlx::query() only — no query!() macros, matching
// every other store in this module.

use serde::{Deserialize, Serialize};
use sqlx::Row;
use sqlx::SqliteConnection;

use crate::persistence::personal_store::{open_personal_db, PersonalStoreError};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Lifecycle values permitted by the entities.status CHECK constraint
/// (personal_002.sql).
///
/// personal_001 originally had active/retired/archived. personal_002 merged
/// that set with decisions.id=502's record-status set
/// (active/deleted_in_source/user_archived/user_deleted) into the single
/// column below, rather than carrying two overlapping status columns:
/// 'user_archived' and 'archived' are the same idea and keep the shorter
/// name, and 'retired' — introduced by cb-01 and never required by any
/// decision — collapsed into 'archived'.
///
/// Meanings:
///   active             normal, appears in all views
///   archived           user explicitly archived it. Recoverable.
///   deleted_in_source  the source no longer has it, QR still does.
///                      Excluded from default views. Recoverable.
///   user_deleted       tombstone. Never re-imported even if it reappears
///                      in source (decisions.id=502, mirroring
///                      rejected_tombstone on entity_facts).
///
/// Validated here so callers get a plain-language Validation error instead
/// of a raw SQLite CHECK failure.
const VALID_STATUSES: &[&str] =
    &["active", "archived", "deleted_in_source", "user_deleted"];

/// Values permitted by the entities.modification_state CHECK constraint
/// (personal_002.sql, decisions.id=502). Governs what a user-triggered
/// source refresh may do to the record.
const VALID_MODIFICATION_STATES: &[&str] =
    &["pristine", "user_modified", "user_created"];

/// Escape character used with LIKE in search_entities. Backslash is not
/// special to SQLite's LIKE by default — it becomes special only via the
/// explicit ESCAPE clause this module always supplies.
const LIKE_ESCAPE: char = '\\';

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// One `entities` row.
///
/// Deliberately defined here rather than in conductor/types.rs: an Entity is
/// a store-layer record, not a Conductor run-context track. EntityFact lives
/// in types.rs because it is loaded into PersonalTrack during INITIALIZE;
/// Entity is not.
///
/// No #[serde(skip)] on any field. Unlike PersonalField/EntityFact, an
/// entity row carries no decrypted secret value — display_name and aliases
/// are identifiers the user chose. Sensitive content about an entity lives
/// in entity_facts, which keeps its own serde_skip discipline.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Entity {
    pub id: String,
    pub entity_type: String,
    pub display_name: String,
    /// Stored as a JSON array TEXT column with a json_valid() CHECK.
    /// Exposed here as a real Vec — callers never hand-build JSON.
    pub aliases: Vec<String>,
    pub parent_entity_id: Option<String>,
    pub status: String,
    /// pristine / user_modified / user_created (decisions.id=502).
    /// Governs what a user-triggered source refresh may do to this record.
    /// Defaults to user_created — a record with no import origin is QR's own.
    pub modification_state: String,
    /// source_registry.id this record was imported from, or None for
    /// QR-origin records (decisions.id=502). Written by cb-11's
    /// source_registry_store, not by create_entity.
    pub source_registry_id: Option<String>,
    /// The record's URL at its source, when it has one — decisions.id=502's
    /// highest-confidence dedup match basis.
    pub source_url: Option<String>,
    pub created_at: String,
    /// Stored as a JSON object TEXT column with a json_valid() CHECK.
    pub extra_metadata: serde_json::Value,
    /// decisions.id=513 (D6-471) Layer 2 flag. When true: identifying facts
    /// removed on Ambient and Boundary surfaces; generic title substituted.
    /// Full semantics owned by decisions.id=513 (P4 -- One Home), evaluated
    /// by conductor::visibility::evaluate_object_visibility. Defaults false
    /// (personal_003.sql).
    pub redact_identification: bool,
    /// decisions.id=513 (D6-471) Layer 2 flag. When true: suppressed
    /// entirely from Ambient surfaces; Direct-surface navigation unaffected.
    /// Full semantics owned by decisions.id=513 (P4 -- One Home). Defaults
    /// false (personal_003.sql).
    pub hide_from_shared_surfaces: bool,
}

impl crate::conductor::visibility::VisibilityFlags for Entity {
    fn redact_identification(&self) -> bool {
        self.redact_identification
    }
    fn hide_from_shared_surfaces(&self) -> bool {
        self.hide_from_shared_surfaces
    }
}

/// Parent-hierarchy filter. A plain Option<String> could not distinguish
/// "don't filter on parent" from "only top-level entities (parent IS NULL)",
/// so the distinction is made explicit in the type.
#[derive(Debug, Clone, Default, PartialEq)]
pub enum ParentFilter {
    /// No constraint on parent_entity_id.
    #[default]
    Any,
    /// Only top-level entities (parent_entity_id IS NULL).
    Root,
    /// Only direct children of the given entity id.
    Under(String),
}

/// Structured filter for list_entities / search_entities.
///
/// entity_type + status together hit idx_entities_type_status
/// (personal_001.sql) directly. Every field is optional; Default is
/// "no constraints", which lists the whole table.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct EntityFilter {
    pub entity_type: Option<String>,
    pub status: Option<String>,
    pub parent: ParentFilter,
}

impl EntityFilter {
    /// The overwhelmingly common case: active records of one type.
    pub fn active_of_type(entity_type: &str) -> Self {
        Self {
            entity_type: Some(entity_type.to_owned()),
            status: Some("active".to_owned()),
            parent: ParentFilter::Any,
        }
    }
}

/// Read-only impact report for a prospective change to an entity — cb-01's
/// "cascade-check (does this change ripple to dependent records)".
///
/// entity_relationships is NOT counted: personal_001.sql declares that table
/// a no-reads/no-writes R1 stub. When relationship-tracking is built post-R1,
/// a relationship_count field belongs here.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EntityCascade {
    pub entity_id: String,
    /// entity_facts rows for this entity with valid_until IS NULL.
    pub active_fact_count: i64,
    /// entity_facts rows for this entity already superseded
    /// (valid_until IS NOT NULL). Counted separately because these are
    /// historical provenance, not live context.
    pub superseded_fact_count: i64,
    /// Entities naming this entity as parent_entity_id.
    pub child_entity_count: i64,
}

impl EntityCascade {
    /// True when nothing depends on this entity — a change to it ripples
    /// nowhere.
    pub fn is_isolated(&self) -> bool {
        self.active_fact_count == 0
            && self.superseded_fact_count == 0
            && self.child_entity_count == 0
    }
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Map one `entities` row to an Entity.
///
/// aliases and extra_metadata are stored as TEXT with json_valid() CHECK
/// constraints, so malformed JSON cannot normally reach this point. If it
/// somehow does, the row is rejected with a Validation error naming the
/// column rather than silently degrading to a default — a store that quietly
/// swallows corrupt rows hides the corruption.
fn row_to_entity(row: &sqlx::sqlite::SqliteRow) -> Result<Entity, PersonalStoreError> {
    let id: String = row.try_get("id")?;
    let aliases_json: String = row.try_get("aliases")?;
    let metadata_json: String = row.try_get("extra_metadata")?;

    let aliases: Vec<String> = serde_json::from_str(&aliases_json).map_err(|e| {
        PersonalStoreError::Validation(format!(
            "entities.aliases for id '{id}' is not a JSON array of strings: {e}"
        ))
    })?;

    let extra_metadata: serde_json::Value =
        serde_json::from_str(&metadata_json).map_err(|e| {
            PersonalStoreError::Validation(format!(
                "entities.extra_metadata for id '{id}' is not valid JSON: {e}"
            ))
        })?;

    let redact_identification_int: i64 = row.try_get("redact_identification")?;
    let hide_from_shared_surfaces_int: i64 = row.try_get("hide_from_shared_surfaces")?;

    Ok(Entity {
        id,
        entity_type: row.try_get("entity_type")?,
        display_name: row.try_get("display_name")?,
        aliases,
        parent_entity_id: row.try_get("parent_entity_id")?,
        status: row.try_get("status")?,
        modification_state: row.try_get("modification_state")?,
        source_registry_id: row.try_get("source_registry_id")?,
        source_url: row.try_get("source_url")?,
        created_at: row.try_get("created_at")?,
        extra_metadata,
        redact_identification: redact_identification_int != 0,
        hide_from_shared_surfaces: hide_from_shared_surfaces_int != 0,
    })
}

/// Reject values the entities CHECK constraints would reject anyway, but
/// with a plain-language message instead of a raw SQLite CHECK failure.
fn validate_status(status: &str) -> Result<(), PersonalStoreError> {
    if !VALID_STATUSES.contains(&status) {
        return Err(PersonalStoreError::Validation(format!(
            "Unknown entity status '{status}'. Must be one of: {}.",
            VALID_STATUSES.join(", ")
        )));
    }
    Ok(())
}

fn validate_modification_state(state: &str) -> Result<(), PersonalStoreError> {
    if !VALID_MODIFICATION_STATES.contains(&state) {
        return Err(PersonalStoreError::Validation(format!(
            "Unknown modification_state '{state}'. Must be one of: {}.",
            VALID_MODIFICATION_STATES.join(", ")
        )));
    }
    Ok(())
}

fn validate_identity(entity_type: &str, display_name: &str) -> Result<(), PersonalStoreError> {
    if entity_type.trim().is_empty() {
        return Err(PersonalStoreError::Validation(
            "entity_type is required and cannot be blank.".to_owned(),
        ));
    }
    if display_name.trim().is_empty() {
        return Err(PersonalStoreError::Validation(
            "display_name is required and cannot be blank.".to_owned(),
        ));
    }
    Ok(())
}

/// Escape LIKE metacharacters in user-supplied search text so a query
/// containing % or _ matches literally instead of acting as a wildcard.
/// Paired with an explicit ESCAPE clause at every call site.
fn escape_like(term: &str) -> String {
    let mut out = String::with_capacity(term.len());
    for ch in term.chars() {
        if ch == '%' || ch == '_' || ch == LIKE_ESCAPE {
            out.push(LIKE_ESCAPE);
        }
        out.push(ch);
    }
    out
}

/// Build the WHERE fragment for a structured filter, plus the bind values in
/// placeholder order. Returns ("", vec![]) for an unconstrained filter.
///
/// Binds are returned rather than interpolated — no filter value is ever
/// concatenated into SQL text.
fn filter_clause(filter: &EntityFilter) -> (String, Vec<String>) {
    let mut clauses: Vec<&str> = Vec::new();
    let mut binds: Vec<String> = Vec::new();

    if let Some(t) = &filter.entity_type {
        clauses.push("entity_type = ?");
        binds.push(t.clone());
    }
    if let Some(s) = &filter.status {
        clauses.push("status = ?");
        binds.push(s.clone());
    }
    match &filter.parent {
        ParentFilter::Any => {}
        ParentFilter::Root => clauses.push("parent_entity_id IS NULL"),
        ParentFilter::Under(parent_id) => {
            clauses.push("parent_entity_id = ?");
            binds.push(parent_id.clone());
        }
    }

    if clauses.is_empty() {
        (String::new(), binds)
    } else {
        (format!(" WHERE {}", clauses.join(" AND ")), binds)
    }
}

/// Columns selected by every read path — kept in one place so the SELECT
/// list and row_to_entity() cannot drift apart.
const ENTITY_COLUMNS: &str =
    "id, entity_type, display_name, aliases, parent_entity_id, status, \
     modification_state, source_registry_id, source_url, created_at, \
     extra_metadata, redact_identification, hide_from_shared_surfaces";

// ---------------------------------------------------------------------------
// Create
// ---------------------------------------------------------------------------

/// Insert a new entity. Returns the generated id (UUID v4).
///
/// created_at is written explicitly via providers::utils::now() (RFC3339)
/// rather than left to the column DEFAULT, matching how
/// personal_store::create_entity_fact_with_provenance timestamps its rows.
/// The column DEFAULT uses a different format; letting both paths write
/// would produce two timestamp formats in one column.
///
/// Entities are mutable records (no valid_until / supersede chain) — unlike
/// entity_facts, which are immutable and superseded rather than updated.
pub async fn create_entity(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
    entity_type: &str,
    display_name: &str,
    aliases: &[String],
    parent_entity_id: Option<&str>,
    extra_metadata: Option<serde_json::Value>,
) -> Result<String, PersonalStoreError> {
    let mut conn = open_personal_db(user_id, persona_id, key_hex).await?;
    create_entity_conn(
        &mut conn,
        entity_type,
        display_name,
        aliases,
        parent_entity_id,
        extra_metadata,
    )
    .await
}

pub(crate) async fn create_entity_conn(
    conn: &mut SqliteConnection,
    entity_type: &str,
    display_name: &str,
    aliases: &[String],
    parent_entity_id: Option<&str>,
    extra_metadata: Option<serde_json::Value>,
) -> Result<String, PersonalStoreError> {
    validate_identity(entity_type, display_name)?;

    let new_id = uuid::Uuid::new_v4().to_string();
    let aliases_json = serde_json::to_string(aliases)
        .map_err(|e| PersonalStoreError::Validation(format!("aliases not serializable: {e}")))?;
    let metadata_json =
        serde_json::to_string(&extra_metadata.unwrap_or(serde_json::json!({}))).map_err(|e| {
            PersonalStoreError::Validation(format!("extra_metadata not serializable: {e}"))
        })?;

    sqlx::query(
        "INSERT INTO entities
         (id, entity_type, display_name, aliases, parent_entity_id, status,
          created_at, extra_metadata)
         VALUES (?, ?, ?, ?, ?, 'active', ?, ?)",
    )
    .bind(&new_id)
    .bind(entity_type)
    .bind(display_name)
    .bind(&aliases_json)
    .bind(parent_entity_id)
    .bind(crate::providers::utils::now())
    .bind(&metadata_json)
    .execute(&mut *conn)
    .await?;

    Ok(new_id)
}

// ---------------------------------------------------------------------------
// Read (single)
// ---------------------------------------------------------------------------

/// Fetch one entity by id. Ok(None) when no such row exists — absence is a
/// normal result, not an error.
pub async fn get_entity(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
    entity_id: &str,
) -> Result<Option<Entity>, PersonalStoreError> {
    let mut conn = open_personal_db(user_id, persona_id, key_hex).await?;
    get_entity_conn(&mut conn, entity_id).await
}

pub(crate) async fn get_entity_conn(
    conn: &mut SqliteConnection,
    entity_id: &str,
) -> Result<Option<Entity>, PersonalStoreError> {
    let row = sqlx::query(&format!(
        "SELECT {ENTITY_COLUMNS} FROM entities WHERE id = ?"
    ))
    .bind(entity_id)
    .fetch_optional(&mut *conn)
    .await?;

    match row {
        Some(r) => Ok(Some(row_to_entity(&r)?)),
        None => Ok(None),
    }
}

// ---------------------------------------------------------------------------
// Update
// ---------------------------------------------------------------------------

/// Partial update payload. Every field is optional; None means "leave
/// unchanged".
///
/// parent_entity_id is doubly wrapped on purpose:
///   None             — leave the parent as it is
///   Some(None)       — clear the parent (promote to top level)
///   Some(Some(id))   — set the parent to id
///
/// entity_type is deliberately absent. An entity's type determines what its
/// entity_facts field_names mean, so retyping a record silently invalidates
/// every fact hanging off it. If a real need appears, it should arrive as an
/// explicit retype path that consults check_entity_cascade first — not as a
/// quiet field on the general update struct.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct EntityUpdate {
    pub display_name: Option<String>,
    pub aliases: Option<Vec<String>>,
    pub parent_entity_id: Option<Option<String>>,
    pub status: Option<String>,
    /// pristine / user_modified / user_created (decisions.id=502).
    /// Transitions are owned by cb-11's source_registry_store, which knows
    /// what a refresh or an import means. Exposed here because the column
    /// lives on this table and a direct set is occasionally the honest
    /// operation.
    pub modification_state: Option<String>,
    pub extra_metadata: Option<serde_json::Value>,
    /// decisions.id=513 (D6-471) Layer 2 flag. User-adjustable at any time
    /// (decisions.id=513: "User controls both per object at any time").
    pub redact_identification: Option<bool>,
    /// decisions.id=513 (D6-471) Layer 2 flag. User-adjustable at any time.
    pub hide_from_shared_surfaces: Option<bool>,
}

impl EntityUpdate {
    fn is_empty(&self) -> bool {
        self.display_name.is_none()
            && self.aliases.is_none()
            && self.parent_entity_id.is_none()
            && self.status.is_none()
            && self.modification_state.is_none()
            && self.extra_metadata.is_none()
            && self.redact_identification.is_none()
            && self.hide_from_shared_surfaces.is_none()
    }
}

/// Apply a partial update to one entity.
///
/// Errors (rather than silently no-opping) when the id does not exist —
/// a caller updating a record that isn't there has a bug worth surfacing.
pub async fn update_entity(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
    entity_id: &str,
    update: &EntityUpdate,
) -> Result<(), PersonalStoreError> {
    let mut conn = open_personal_db(user_id, persona_id, key_hex).await?;
    update_entity_conn(&mut conn, entity_id, update).await
}

pub(crate) async fn update_entity_conn(
    conn: &mut SqliteConnection,
    entity_id: &str,
    update: &EntityUpdate,
) -> Result<(), PersonalStoreError> {
    if update.is_empty() {
        return Err(PersonalStoreError::Validation(
            "update_entity called with no fields to change.".to_owned(),
        ));
    }
    if let Some(status) = &update.status {
        validate_status(status)?;
    }
    if let Some(state) = &update.modification_state {
        validate_modification_state(state)?;
    }
    if let Some(name) = &update.display_name {
        if name.trim().is_empty() {
            return Err(PersonalStoreError::Validation(
                "display_name cannot be blank.".to_owned(),
            ));
        }
    }
    if let Some(Some(parent)) = &update.parent_entity_id {
        if parent == entity_id {
            return Err(PersonalStoreError::Validation(
                "An entity cannot be its own parent.".to_owned(),
            ));
        }
    }

    // Every bind is modelled as Option<String> so one uniform bind loop
    // covers both value columns and the nullable parent_entity_id.
    let mut sets: Vec<&str> = Vec::new();
    let mut binds: Vec<Option<String>> = Vec::new();

    if let Some(name) = &update.display_name {
        sets.push("display_name = ?");
        binds.push(Some(name.clone()));
    }
    if let Some(aliases) = &update.aliases {
        let json = serde_json::to_string(aliases).map_err(|e| {
            PersonalStoreError::Validation(format!("aliases not serializable: {e}"))
        })?;
        sets.push("aliases = ?");
        binds.push(Some(json));
    }
    if let Some(parent) = &update.parent_entity_id {
        sets.push("parent_entity_id = ?");
        binds.push(parent.clone());
    }
    if let Some(status) = &update.status {
        sets.push("status = ?");
        binds.push(Some(status.clone()));
    }
    if let Some(state) = &update.modification_state {
        sets.push("modification_state = ?");
        binds.push(Some(state.clone()));
    }
    if let Some(metadata) = &update.extra_metadata {
        let json = serde_json::to_string(metadata).map_err(|e| {
            PersonalStoreError::Validation(format!("extra_metadata not serializable: {e}"))
        })?;
        sets.push("extra_metadata = ?");
        binds.push(Some(json));
    }
    if let Some(flag) = update.redact_identification {
        // Bound as "0"/"1" text against the INTEGER column -- SQLite's
        // dynamic typing coerces the TEXT literal into the column's INTEGER
        // affinity, and the CHECK (... IN (0, 1)) constraint accepts the
        // coerced value. Kept as a string here rather than restructuring
        // the whole bind Vec's element type for two bool columns.
        sets.push("redact_identification = ?");
        binds.push(Some(if flag { "1" } else { "0" }.to_owned()));
    }
    if let Some(flag) = update.hide_from_shared_surfaces {
        sets.push("hide_from_shared_surfaces = ?");
        binds.push(Some(if flag { "1" } else { "0" }.to_owned()));
    }

    let sql = format!("UPDATE entities SET {} WHERE id = ?", sets.join(", "));
    let mut q = sqlx::query(&sql);
    for b in binds {
        q = q.bind(b);
    }
    let result = q.bind(entity_id).execute(&mut *conn).await?;

    if result.rows_affected() == 0 {
        return Err(PersonalStoreError::Validation(format!(
            "No entity with id '{entity_id}' — nothing was updated."
        )));
    }

    Ok(())
}

/// Move an entity to a non-active status — 'archived' (user archived it),
/// 'deleted_in_source' (the source dropped it, QR kept it), or
/// 'user_deleted' (tombstone, never re-imported; decisions.id=502). This is
/// the non-destructive alternative to deletion: facts, children, and history
/// all survive, and the record simply stops appearing in
/// EntityFilter::active_of_type() results.
///
/// See the module header for why no hard-delete path exists.
pub async fn retire_entity(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
    entity_id: &str,
    status: &str,
) -> Result<(), PersonalStoreError> {
    let mut conn = open_personal_db(user_id, persona_id, key_hex).await?;
    retire_entity_conn(&mut conn, entity_id, status).await
}

pub(crate) async fn retire_entity_conn(
    conn: &mut SqliteConnection,
    entity_id: &str,
    status: &str,
) -> Result<(), PersonalStoreError> {
    if status == "active" {
        return Err(PersonalStoreError::Validation(
            "retire_entity moves an entity out of 'active' — use update_entity \
             to reactivate one."
                .to_owned(),
        ));
    }
    validate_status(status)?;

    update_entity_conn(
        conn,
        entity_id,
        &EntityUpdate {
            status: Some(status.to_owned()),
            ..Default::default()
        },
    )
    .await
}

// ---------------------------------------------------------------------------
// List (structured filter)
// ---------------------------------------------------------------------------

/// List entities matching a structured filter, ordered by display_name.
/// An unconstrained EntityFilter::default() returns every row.
///
/// entity_type + status filters are served by idx_entities_type_status
/// (personal_001.sql).
pub async fn list_entities(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
    filter: &EntityFilter,
) -> Result<Vec<Entity>, PersonalStoreError> {
    let mut conn = open_personal_db(user_id, persona_id, key_hex).await?;
    list_entities_conn(&mut conn, filter).await
}

pub(crate) async fn list_entities_conn(
    conn: &mut SqliteConnection,
    filter: &EntityFilter,
) -> Result<Vec<Entity>, PersonalStoreError> {
    if let Some(status) = &filter.status {
        validate_status(status)?;
    }

    let (where_clause, binds) = filter_clause(filter);
    let sql = format!(
        "SELECT {ENTITY_COLUMNS} FROM entities{where_clause} ORDER BY display_name"
    );

    let mut q = sqlx::query(&sql);
    for b in binds {
        q = q.bind(b);
    }
    let rows = q.fetch_all(&mut *conn).await?;

    let mut out = Vec::with_capacity(rows.len());
    for row in &rows {
        out.push(row_to_entity(row)?);
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Search (structured filter + keyword)
// ---------------------------------------------------------------------------

/// Keyword search over display_name and aliases, narrowed by the same
/// structured filter list_entities accepts. Results are ordered
/// display_name-first so callers get a stable, presentable order.
///
/// MATCH SEMANTICS — deliberately simple, and worth knowing before relying
/// on it:
///   * Substring match, not token or prefix match: "ill" matches "Vanilla".
///   * Case-insensitive for ASCII only. SQLite's built-in LIKE and lower()
///     do not case-fold non-ASCII characters without ICU, which this build
///     does not link.
///   * aliases is matched as raw JSON array text. A search term containing
///     JSON punctuation (a quote, a bracket) can therefore match structural
///     characters rather than content. LIKE metacharacters (% and _) ARE
///     escaped, so those behave literally.
///   * An empty or whitespace-only query is rejected rather than silently
///     returning the whole table — callers wanting everything should call
///     list_entities.
///
/// No FTS5 virtual table exists in personal_001.sql. Ranked or tokenised
/// search would be a schema change and is out of cb-01's scope; if adopter
/// Focuses outgrow substring matching, that is the upgrade path.
pub async fn search_entities(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
    query: &str,
    filter: &EntityFilter,
) -> Result<Vec<Entity>, PersonalStoreError> {
    let mut conn = open_personal_db(user_id, persona_id, key_hex).await?;
    search_entities_conn(&mut conn, query, filter).await
}

pub(crate) async fn search_entities_conn(
    conn: &mut SqliteConnection,
    query: &str,
    filter: &EntityFilter,
) -> Result<Vec<Entity>, PersonalStoreError> {
    if query.trim().is_empty() {
        return Err(PersonalStoreError::Validation(
            "Search query cannot be empty — use list_entities to retrieve \
             everything matching a filter."
                .to_owned(),
        ));
    }
    if let Some(status) = &filter.status {
        validate_status(status)?;
    }

    let (where_clause, filter_binds) = filter_clause(filter);
    let joiner = if where_clause.is_empty() { " WHERE" } else { " AND" };
    let pattern = format!("%{}%", escape_like(&query.trim().to_lowercase()));

    let sql = format!(
        "SELECT {ENTITY_COLUMNS} FROM entities{where_clause}{joiner} \
         (lower(display_name) LIKE ? ESCAPE '{LIKE_ESCAPE}' \
          OR lower(aliases) LIKE ? ESCAPE '{LIKE_ESCAPE}') \
         ORDER BY display_name"
    );

    let mut q = sqlx::query(&sql);
    for b in filter_binds {
        q = q.bind(b);
    }
    let rows = q
        .bind(&pattern)
        .bind(&pattern)
        .fetch_all(&mut *conn)
        .await?;

    let mut out = Vec::with_capacity(rows.len());
    for row in &rows {
        out.push(row_to_entity(row)?);
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Cascade check
// ---------------------------------------------------------------------------

/// Report what depends on an entity, so a caller can surface the blast
/// radius of a change before making it — cb-01's cascade-check capability.
///
/// Read-only by design. It reports; it never acts. Nothing in this module
/// calls it automatically, because "surface, don't auto-resolve" is the
/// project's standing posture on destructive-adjacent operations.
///
/// Returns Ok(None) when the entity does not exist, distinguishing "no such
/// record" from "record exists and nothing depends on it"
/// (Some(cascade) where cascade.is_isolated()).
pub async fn check_entity_cascade(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
    entity_id: &str,
) -> Result<Option<EntityCascade>, PersonalStoreError> {
    let mut conn = open_personal_db(user_id, persona_id, key_hex).await?;
    check_entity_cascade_conn(&mut conn, entity_id).await
}

pub(crate) async fn check_entity_cascade_conn(
    conn: &mut SqliteConnection,
    entity_id: &str,
) -> Result<Option<EntityCascade>, PersonalStoreError> {
    let exists: Option<(String,)> = sqlx::query_as("SELECT id FROM entities WHERE id = ?")
        .bind(entity_id)
        .fetch_optional(&mut *conn)
        .await?;
    if exists.is_none() {
        return Ok(None);
    }

    let (active_fact_count,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM entity_facts \
         WHERE entity_id = ? AND valid_until IS NULL",
    )
    .bind(entity_id)
    .fetch_one(&mut *conn)
    .await?;

    let (superseded_fact_count,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM entity_facts \
         WHERE entity_id = ? AND valid_until IS NOT NULL",
    )
    .bind(entity_id)
    .fetch_one(&mut *conn)
    .await?;

    let (child_entity_count,): (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM entities WHERE parent_entity_id = ?")
            .bind(entity_id)
            .fetch_one(&mut *conn)
            .await?;

    Ok(Some(EntityCascade {
        entity_id: entity_id.to_owned(),
        active_fact_count,
        superseded_fact_count,
        child_entity_count,
    }))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------
//
// These are the first DB-backed tests in the persistence layer. They run the
// real personal_001.sql schema — including its CHECK constraints, partial
// unique indexes, and provenance triggers — against an in-memory SQLite
// database, so a schema change that breaks this block fails here rather than
// at runtime. No QR_DATA_ROOT and no SQLCipher key are involved: encryption
// is a property of the file on disk, not of the SQL these functions issue.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persistence::migrations::parse_statements;
    use sqlx::sqlite::SqliteConnectOptions;
    use sqlx::ConnectOptions;

    const PERSONAL_SCHEMA_V1: &str = include_str!("../../schema/personal_001.sql");
    const PERSONAL_SCHEMA_V2: &str = include_str!("../../schema/personal_002.sql");
    const PERSONAL_SCHEMA_V3: &str = include_str!("../../schema/personal_003.sql");

    /// In-memory personal.db with the real schema applied, v1 through v3 —
    /// the same order and the same statement splitter the migration runner
    /// uses, so the v2 entities rebuild and the v3 flag-column additions are
    /// both exercised on every single test.
    async fn test_db() -> SqliteConnection {
        let mut conn = SqliteConnectOptions::new()
            .filename(":memory:")
            .connect()
            .await
            .expect("in-memory connection failed");

        for schema in [PERSONAL_SCHEMA_V1, PERSONAL_SCHEMA_V2, PERSONAL_SCHEMA_V3] {
            for stmt in parse_statements(schema) {
                sqlx::query(&stmt)
                    .execute(&mut conn)
                    .await
                    .unwrap_or_else(|e| panic!("schema statement failed: {e}\n{stmt}"));
            }
        }
        conn
    }

    /// Insert an entity_facts row directly. entity_facts is personal_store's
    /// table, not this module's — the cascade tests need dependent rows to
    /// count, and going through SQL keeps this module from growing a write
    /// path it should not own.
    async fn insert_fact(
        conn: &mut SqliteConnection,
        entity_id: &str,
        field_name: &str,
        valid_until: Option<&str>,
    ) {
        sqlx::query(
            "INSERT INTO entity_facts
             (id, entity_id, field_name, field_value, sensitivity, created_at,
              valid_until, source_persona_id)
             VALUES (?, ?, ?, ?, 'general', '2026-07-25T00:00:00Z', ?, 'persona-1')",
        )
        .bind(uuid::Uuid::new_v4().to_string())
        .bind(entity_id)
        .bind(field_name)
        .bind("value")
        .bind(valid_until)
        .execute(&mut *conn)
        .await
        .expect("fact insert failed");
    }

    // -- create / read ------------------------------------------------------

    #[tokio::test]
    async fn create_then_get_round_trips_all_fields() {
        let mut conn = test_db().await;
        let id = create_entity_conn(
            &mut conn,
            "recipe",
            "Vanilla Custard",
            &["custard".to_owned(), "crème anglaise".to_owned()],
            None,
            Some(serde_json::json!({"servings": 4})),
        )
        .await
        .expect("create failed");

        let e = get_entity_conn(&mut conn, &id)
            .await
            .expect("get failed")
            .expect("entity must exist");

        assert_eq!(e.id, id);
        assert_eq!(e.entity_type, "recipe");
        assert_eq!(e.display_name, "Vanilla Custard");
        assert_eq!(e.aliases, vec!["custard", "crème anglaise"]);
        assert_eq!(e.parent_entity_id, None);
        assert_eq!(e.status, "active");
        assert_eq!(
            e.modification_state, "user_created",
            "a record with no import origin is QR's own (decisions.id=502)"
        );
        assert_eq!(e.extra_metadata, serde_json::json!({"servings": 4}));
        assert!(!e.created_at.is_empty(), "created_at must be written");
    }

    #[tokio::test]
    async fn get_missing_entity_is_none_not_error() {
        let mut conn = test_db().await;
        let result = get_entity_conn(&mut conn, "no-such-id").await;
        assert!(matches!(result, Ok(None)), "absence is not an error");
    }

    #[tokio::test]
    async fn create_rejects_blank_identity_fields() {
        let mut conn = test_db().await;
        assert!(
            create_entity_conn(&mut conn, "  ", "Name", &[], None, None)
                .await
                .is_err(),
            "blank entity_type must be rejected"
        );
        assert!(
            create_entity_conn(&mut conn, "recipe", "   ", &[], None, None)
                .await
                .is_err(),
            "blank display_name must be rejected"
        );
    }

    #[tokio::test]
    async fn create_defaults_empty_aliases_and_metadata_to_valid_json() {
        let mut conn = test_db().await;
        let id = create_entity_conn(&mut conn, "device", "Router", &[], None, None)
            .await
            .expect("create failed");
        let e = get_entity_conn(&mut conn, &id).await.unwrap().unwrap();
        assert!(e.aliases.is_empty());
        assert_eq!(e.extra_metadata, serde_json::json!({}));
    }

    #[tokio::test]
    async fn create_defaults_both_visibility_flags_to_false() {
        // decisions.id=513: both flags default false.
        let mut conn = test_db().await;
        let id = create_entity_conn(&mut conn, "recipe", "Plain Toast", &[], None, None)
            .await
            .expect("create failed");
        let e = get_entity_conn(&mut conn, &id).await.unwrap().unwrap();
        assert!(!e.redact_identification);
        assert!(!e.hide_from_shared_surfaces);
    }

    #[tokio::test]
    async fn update_entity_sets_each_visibility_flag_independently() {
        // decisions.id=513: "Both flags independent and combinable."
        let mut conn = test_db().await;
        let id = create_entity_conn(&mut conn, "device", "Home Router", &[], None, None)
            .await
            .expect("create failed");

        update_entity_conn(
            &mut conn,
            &id,
            &EntityUpdate {
                redact_identification: Some(true),
                ..Default::default()
            },
        )
        .await
        .expect("update failed");

        let e = get_entity_conn(&mut conn, &id).await.unwrap().unwrap();
        assert!(e.redact_identification);
        assert!(
            !e.hide_from_shared_surfaces,
            "setting one flag must not touch the other"
        );

        update_entity_conn(
            &mut conn,
            &id,
            &EntityUpdate {
                hide_from_shared_surfaces: Some(true),
                ..Default::default()
            },
        )
        .await
        .expect("update failed");

        let e = get_entity_conn(&mut conn, &id).await.unwrap().unwrap();
        assert!(e.redact_identification, "first flag must persist");
        assert!(e.hide_from_shared_surfaces);

        // Explicit false must also apply, not be treated as "leave unchanged"
        // — Option<bool>::Some(false) is a real value here, unlike a plain
        // Option<T> field where None means "no change."
        update_entity_conn(
            &mut conn,
            &id,
            &EntityUpdate {
                redact_identification: Some(false),
                ..Default::default()
            },
        )
        .await
        .expect("update failed");

        let e = get_entity_conn(&mut conn, &id).await.unwrap().unwrap();
        assert!(!e.redact_identification, "explicit false must be applied");
        assert!(e.hide_from_shared_surfaces, "untouched flag must persist");
    }

    // -- structured filter --------------------------------------------------

    /// Small fixture: two recipes (one retired), one device, and a child
    /// recipe under the first. Returns (parent_recipe_id, child_recipe_id).
    async fn seed_filter_fixture(conn: &mut SqliteConnection) -> (String, String) {
        let parent = create_entity_conn(
            conn,
            "recipe",
            "Bread",
            &["loaf".to_owned()],
            None,
            None,
        )
        .await
        .unwrap();
        let child = create_entity_conn(
            conn,
            "recipe",
            "Sourdough Starter",
            &[],
            Some(&parent),
            None,
        )
        .await
        .unwrap();
        let retired = create_entity_conn(conn, "recipe", "Old Scones", &[], None, None)
            .await
            .unwrap();
        retire_entity_conn(conn, &retired, "archived").await.unwrap();
        create_entity_conn(conn, "device", "Oven", &[], None, None)
            .await
            .unwrap();
        (parent, child)
    }

    #[tokio::test]
    async fn list_with_default_filter_returns_everything() {
        let mut conn = test_db().await;
        seed_filter_fixture(&mut conn).await;
        let all = list_entities_conn(&mut conn, &EntityFilter::default())
            .await
            .unwrap();
        assert_eq!(all.len(), 4);
    }

    #[tokio::test]
    async fn list_orders_by_display_name() {
        let mut conn = test_db().await;
        seed_filter_fixture(&mut conn).await;
        let all = list_entities_conn(&mut conn, &EntityFilter::default())
            .await
            .unwrap();
        let names: Vec<&str> = all.iter().map(|e| e.display_name.as_str()).collect();
        assert_eq!(names, vec!["Bread", "Old Scones", "Oven", "Sourdough Starter"]);
    }

    #[tokio::test]
    async fn list_filters_by_type_and_status_together() {
        let mut conn = test_db().await;
        seed_filter_fixture(&mut conn).await;

        let active_recipes =
            list_entities_conn(&mut conn, &EntityFilter::active_of_type("recipe"))
                .await
                .unwrap();
        let names: Vec<&str> = active_recipes
            .iter()
            .map(|e| e.display_name.as_str())
            .collect();
        assert_eq!(
            names,
            vec!["Bread", "Sourdough Starter"],
            "the retired recipe and the device must both be excluded"
        );
    }

    #[tokio::test]
    async fn list_parent_filter_distinguishes_root_from_children() {
        let mut conn = test_db().await;
        let (parent, child) = seed_filter_fixture(&mut conn).await;

        let roots = list_entities_conn(
            &mut conn,
            &EntityFilter {
                parent: ParentFilter::Root,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(roots.len(), 3, "everything except the one child");
        assert!(roots.iter().all(|e| e.parent_entity_id.is_none()));

        let children = list_entities_conn(
            &mut conn,
            &EntityFilter {
                parent: ParentFilter::Under(parent.clone()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(children.len(), 1);
        assert_eq!(children[0].id, child);
    }

    #[tokio::test]
    async fn list_rejects_unknown_status_filter() {
        let mut conn = test_db().await;
        assert!(
            list_entities_conn(
                &mut conn,
                &EntityFilter {
                    status: Some("deleted".to_owned()),
                    ..Default::default()
                },
            )
            .await
            .is_err(),
            "a status outside the CHECK constraint must be rejected up front"
        );
    }

    // -- search -------------------------------------------------------------

    #[tokio::test]
    async fn search_matches_display_name_case_insensitively() {
        let mut conn = test_db().await;
        seed_filter_fixture(&mut conn).await;
        let hits = search_entities_conn(&mut conn, "BREAD", &EntityFilter::default())
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].display_name, "Bread");
    }

    #[tokio::test]
    async fn search_matches_an_alias_not_present_in_display_name() {
        let mut conn = test_db().await;
        seed_filter_fixture(&mut conn).await;
        let hits = search_entities_conn(&mut conn, "loaf", &EntityFilter::default())
            .await
            .unwrap();
        assert_eq!(hits.len(), 1, "alias-only match must be found");
        assert_eq!(hits[0].display_name, "Bread");
    }

    #[tokio::test]
    async fn search_matches_substrings_not_only_prefixes() {
        let mut conn = test_db().await;
        create_entity_conn(&mut conn, "recipe", "Vanilla Custard", &[], None, None)
            .await
            .unwrap();
        let hits = search_entities_conn(&mut conn, "ill", &EntityFilter::default())
            .await
            .unwrap();
        assert_eq!(hits.len(), 1, "substring match is the documented behaviour");
    }

    #[tokio::test]
    async fn search_applies_the_structured_filter_too() {
        let mut conn = test_db().await;
        create_entity_conn(&mut conn, "recipe", "Iron Skillet Cornbread", &[], None, None)
            .await
            .unwrap();
        create_entity_conn(&mut conn, "device", "Iron Skillet", &[], None, None)
            .await
            .unwrap();

        let hits = search_entities_conn(
            &mut conn,
            "iron skillet",
            &EntityFilter::active_of_type("device"),
        )
        .await
        .unwrap();
        assert_eq!(hits.len(), 1, "the recipe must be filtered out by type");
        assert_eq!(hits[0].entity_type, "device");
    }

    #[tokio::test]
    async fn search_treats_like_metacharacters_literally() {
        let mut conn = test_db().await;
        create_entity_conn(&mut conn, "note", "100% Rye", &[], None, None)
            .await
            .unwrap();
        create_entity_conn(&mut conn, "note", "Plain Wheat", &[], None, None)
            .await
            .unwrap();

        // Unescaped, "%" would be a wildcard and match both rows.
        let hits = search_entities_conn(&mut conn, "100%", &EntityFilter::default())
            .await
            .unwrap();
        assert_eq!(hits.len(), 1, "% must match literally, not as a wildcard");
        assert_eq!(hits[0].display_name, "100% Rye");

        // "_" is LIKE's single-character wildcard; escaped, it matches nothing here.
        let underscore_hits = search_entities_conn(&mut conn, "_", &EntityFilter::default())
            .await
            .unwrap();
        assert!(
            underscore_hits.is_empty(),
            "_ must match literally, not any single character"
        );
    }

    #[tokio::test]
    async fn search_rejects_an_empty_query() {
        let mut conn = test_db().await;
        assert!(
            search_entities_conn(&mut conn, "   ", &EntityFilter::default())
                .await
                .is_err(),
            "an empty query must not silently return the whole table"
        );
    }

    #[tokio::test]
    async fn search_with_no_matches_returns_empty_not_error() {
        let mut conn = test_db().await;
        seed_filter_fixture(&mut conn).await;
        let hits = search_entities_conn(&mut conn, "zzzzz", &EntityFilter::default())
            .await
            .unwrap();
        assert!(hits.is_empty());
    }

    // -- update / retire ----------------------------------------------------

    #[tokio::test]
    async fn update_changes_only_the_named_fields() {
        let mut conn = test_db().await;
        let id = create_entity_conn(
            &mut conn,
            "recipe",
            "Bread",
            &["loaf".to_owned()],
            None,
            Some(serde_json::json!({"servings": 2})),
        )
        .await
        .unwrap();

        update_entity_conn(
            &mut conn,
            &id,
            &EntityUpdate {
                display_name: Some("Country Bread".to_owned()),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let e = get_entity_conn(&mut conn, &id).await.unwrap().unwrap();
        assert_eq!(e.display_name, "Country Bread");
        assert_eq!(e.aliases, vec!["loaf"], "aliases must be untouched");
        assert_eq!(e.extra_metadata, serde_json::json!({"servings": 2}));
        assert_eq!(e.status, "active");
    }

    #[tokio::test]
    async fn update_can_set_and_clear_the_parent() {
        let mut conn = test_db().await;
        let parent = create_entity_conn(&mut conn, "recipe", "Bread", &[], None, None)
            .await
            .unwrap();
        let child = create_entity_conn(&mut conn, "recipe", "Starter", &[], None, None)
            .await
            .unwrap();

        update_entity_conn(
            &mut conn,
            &child,
            &EntityUpdate {
                parent_entity_id: Some(Some(parent.clone())),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let e = get_entity_conn(&mut conn, &child).await.unwrap().unwrap();
        assert_eq!(e.parent_entity_id, Some(parent));

        update_entity_conn(
            &mut conn,
            &child,
            &EntityUpdate {
                parent_entity_id: Some(None),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let e = get_entity_conn(&mut conn, &child).await.unwrap().unwrap();
        assert_eq!(e.parent_entity_id, None, "Some(None) must clear the parent");
    }

    #[tokio::test]
    async fn update_rejects_self_parenting() {
        let mut conn = test_db().await;
        let id = create_entity_conn(&mut conn, "recipe", "Bread", &[], None, None)
            .await
            .unwrap();
        assert!(
            update_entity_conn(
                &mut conn,
                &id,
                &EntityUpdate {
                    parent_entity_id: Some(Some(id.clone())),
                    ..Default::default()
                },
            )
            .await
            .is_err(),
            "an entity cannot be its own parent"
        );
    }

    #[tokio::test]
    async fn update_rejects_empty_payload_and_missing_id() {
        let mut conn = test_db().await;
        assert!(
            update_entity_conn(&mut conn, "anything", &EntityUpdate::default())
                .await
                .is_err(),
            "an update with no fields is a caller bug"
        );
        assert!(
            update_entity_conn(
                &mut conn,
                "no-such-id",
                &EntityUpdate {
                    display_name: Some("X".to_owned()),
                    ..Default::default()
                },
            )
            .await
            .is_err(),
            "updating a nonexistent entity must error, not silently no-op"
        );
    }

    #[tokio::test]
    async fn retire_removes_from_active_listing_without_deleting() {
        let mut conn = test_db().await;
        let id = create_entity_conn(&mut conn, "recipe", "Old Scones", &[], None, None)
            .await
            .unwrap();
        insert_fact(&mut conn, &id, "oven_temp", None).await;

        retire_entity_conn(&mut conn, &id, "archived").await.unwrap();

        let active = list_entities_conn(&mut conn, &EntityFilter::active_of_type("recipe"))
            .await
            .unwrap();
        assert!(active.is_empty(), "archived record must leave active listings");

        let still_there = get_entity_conn(&mut conn, &id).await.unwrap();
        assert!(still_there.is_some(), "retire must not delete the row");
        assert_eq!(still_there.unwrap().status, "archived");

        let cascade = check_entity_cascade_conn(&mut conn, &id).await.unwrap().unwrap();
        assert_eq!(cascade.active_fact_count, 1, "facts must survive retirement");
    }

    #[tokio::test]
    async fn retire_rejects_active_and_unknown_statuses() {
        let mut conn = test_db().await;
        let id = create_entity_conn(&mut conn, "recipe", "Bread", &[], None, None)
            .await
            .unwrap();
        assert!(
            retire_entity_conn(&mut conn, &id, "active").await.is_err(),
            "retire_entity is not a reactivation path"
        );
        assert!(
            retire_entity_conn(&mut conn, &id, "deleted").await.is_err(),
            "a status outside the CHECK constraint must be rejected"
        );
    }

    // -- cascade check ------------------------------------------------------

    #[tokio::test]
    async fn cascade_counts_facts_children_and_history_separately() {
        let mut conn = test_db().await;
        let parent = create_entity_conn(&mut conn, "recipe", "Bread", &[], None, None)
            .await
            .unwrap();
        create_entity_conn(&mut conn, "recipe", "Starter", &[], Some(&parent), None)
            .await
            .unwrap();
        create_entity_conn(&mut conn, "recipe", "Levain", &[], Some(&parent), None)
            .await
            .unwrap();

        insert_fact(&mut conn, &parent, "hydration", None).await;
        insert_fact(&mut conn, &parent, "bake_time", None).await;
        insert_fact(&mut conn, &parent, "hydration", Some("2026-07-01T00:00:00Z")).await;

        let cascade = check_entity_cascade_conn(&mut conn, &parent)
            .await
            .unwrap()
            .expect("entity exists");

        assert_eq!(cascade.entity_id, parent);
        assert_eq!(cascade.active_fact_count, 2);
        assert_eq!(cascade.superseded_fact_count, 1);
        assert_eq!(cascade.child_entity_count, 2);
        assert!(!cascade.is_isolated());
    }

    #[tokio::test]
    async fn cascade_distinguishes_isolated_from_nonexistent() {
        let mut conn = test_db().await;
        let id = create_entity_conn(&mut conn, "recipe", "Bread", &[], None, None)
            .await
            .unwrap();

        let isolated = check_entity_cascade_conn(&mut conn, &id)
            .await
            .unwrap()
            .expect("an existing entity with no dependents is Some, not None");
        assert!(isolated.is_isolated());

        let missing = check_entity_cascade_conn(&mut conn, "no-such-id")
            .await
            .unwrap();
        assert!(missing.is_none(), "a nonexistent entity reports None");
    }

    #[tokio::test]
    async fn cascade_does_not_count_another_entitys_dependents() {
        let mut conn = test_db().await;
        let a = create_entity_conn(&mut conn, "recipe", "A", &[], None, None)
            .await
            .unwrap();
        let b = create_entity_conn(&mut conn, "recipe", "B", &[], None, None)
            .await
            .unwrap();
        insert_fact(&mut conn, &b, "hydration", None).await;

        let cascade = check_entity_cascade_conn(&mut conn, &a).await.unwrap().unwrap();
        assert!(cascade.is_isolated(), "A must not see B's facts");
    }

    // -- helpers ------------------------------------------------------------

    #[test]
    fn escape_like_escapes_only_the_metacharacters() {
        assert_eq!(escape_like("plain"), "plain");
        assert_eq!(escape_like("100%"), "100\\%");
        assert_eq!(escape_like("a_b"), "a\\_b");
        assert_eq!(escape_like("back\\slash"), "back\\\\slash");
    }

    #[test]
    fn filter_clause_is_empty_for_an_unconstrained_filter() {
        let (clause, binds) = filter_clause(&EntityFilter::default());
        assert!(clause.is_empty());
        assert!(binds.is_empty());
    }

    #[test]
    fn filter_clause_root_adds_no_bind() {
        let (clause, binds) = filter_clause(&EntityFilter {
            parent: ParentFilter::Root,
            ..Default::default()
        });
        assert!(clause.contains("parent_entity_id IS NULL"));
        assert!(binds.is_empty(), "IS NULL takes no placeholder");
    }

    // -- personal_002 migration ---------------------------------------------

    /// Apply v1 only, so the v2 rebuild can be exercised against real v1 data.
    async fn v1_only_db() -> SqliteConnection {
        let mut conn = SqliteConnectOptions::new()
            .filename(":memory:")
            .connect()
            .await
            .expect("in-memory connection failed");
        for stmt in parse_statements(PERSONAL_SCHEMA_V1) {
            sqlx::query(&stmt).execute(&mut conn).await.unwrap();
        }
        conn
    }

    async fn apply_v2(conn: &mut SqliteConnection) {
        for stmt in parse_statements(PERSONAL_SCHEMA_V2) {
            sqlx::query(&stmt)
                .execute(&mut *conn)
                .await
                .unwrap_or_else(|e| panic!("v2 statement failed: {e}\n{stmt}"));
        }
    }

    #[tokio::test]
    async fn migration_v2_collapses_retired_into_archived_and_preserves_rows() {
        let mut conn = v1_only_db().await;

        // Three v1 rows, one in each v1 status.
        for (id, name, status) in [
            ("e-active", "Active One", "active"),
            ("e-retired", "Retired One", "retired"),
            ("e-archived", "Archived One", "archived"),
        ] {
            sqlx::query(
                "INSERT INTO entities (id, entity_type, display_name, aliases,
                 status, created_at, extra_metadata)
                 VALUES (?, 'recipe', ?, '[\"alias\"]', ?, '2026-07-01T00:00:00Z', '{\"k\":1}')",
            )
            .bind(id)
            .bind(name)
            .bind(status)
            .execute(&mut conn)
            .await
            .expect("v1 insert failed");
        }

        apply_v2(&mut conn).await;
        apply_v3(&mut conn).await;

        let all = list_entities_conn(&mut conn, &EntityFilter::default())
            .await
            .unwrap();
        assert_eq!(all.len(), 3, "no row may be lost in the rebuild");

        let by_id = |id: &str| all.iter().find(|e| e.id == id).unwrap().clone();
        assert_eq!(by_id("e-active").status, "active");
        assert_eq!(
            by_id("e-retired").status,
            "archived",
            "'retired' must collapse into 'archived'"
        );
        assert_eq!(by_id("e-archived").status, "archived");

        // Non-status columns must survive the copy intact.
        let retired = by_id("e-retired");
        assert_eq!(retired.display_name, "Retired One");
        assert_eq!(retired.aliases, vec!["alias"]);
        assert_eq!(retired.created_at, "2026-07-01T00:00:00Z");
        assert_eq!(retired.extra_metadata, serde_json::json!({"k": 1}));
        assert_eq!(
            retired.modification_state, "user_created",
            "pre-existing records have no import origin"
        );
    }

    #[tokio::test]
    async fn migration_v2_leaves_entity_facts_pointing_at_entities() {
        // The rebuild drops `entities` and renames its replacement into
        // place. SQLite rewrites REFERENCES clauses during ALTER TABLE
        // RENAME, so without PRAGMA legacy_alter_table this is exactly where
        // entity_facts' foreign key would silently start naming the
        // temporary table. FK enforcement is off codebase-wide, so nothing
        // would fail at runtime — the damage would only show up in an
        // export or a future migration.
        let mut conn = v1_only_db().await;
        apply_v2(&mut conn).await;

        let (ddl,): (String,) = sqlx::query_as(
            "SELECT sql FROM sqlite_master WHERE type='table' AND name='entity_facts'",
        )
        .fetch_one(&mut conn)
        .await
        .unwrap();

        assert!(
            ddl.contains("REFERENCES entities(id)"),
            "entity_facts must still reference `entities`, got: {ddl}"
        );
        assert!(
            !ddl.contains("entities_v2"),
            "the temporary table name must not leak into entity_facts: {ddl}"
        );
    }

    #[tokio::test]
    async fn migration_v2_creates_the_cb11_tables() {
        let mut conn = test_db().await;
        for table in ["source_registry", "dedup_candidates"] {
            let found: Option<(String,)> = sqlx::query_as(
                "SELECT name FROM sqlite_master WHERE type='table' AND name = ?",
            )
            .bind(table)
            .fetch_optional(&mut conn)
            .await
            .unwrap();
            assert!(found.is_some(), "{table} must exist after v2");
        }
    }

    #[tokio::test]
    async fn status_set_matches_the_v2_check_constraint() {
        let mut conn = test_db().await;
        let id = create_entity_conn(&mut conn, "recipe", "Bread", &[], None, None)
            .await
            .unwrap();

        // Every value this module accepts must also satisfy the DB CHECK —
        // a mismatch would surface as a raw SQLite error instead of a
        // plain-language one.
        for status in VALID_STATUSES {
            update_entity_conn(
                &mut conn,
                &id,
                &EntityUpdate {
                    status: Some((*status).to_owned()),
                    ..Default::default()
                },
            )
            .await
            .unwrap_or_else(|e| panic!("status '{status}' rejected by the DB: {e}"));
        }
        assert!(
            !VALID_STATUSES.contains(&"retired"),
            "'retired' was collapsed into 'archived' by personal_002"
        );
    }

    #[tokio::test]
    async fn modification_state_set_matches_the_v2_check_constraint() {
        let mut conn = test_db().await;
        let id = create_entity_conn(&mut conn, "recipe", "Bread", &[], None, None)
            .await
            .unwrap();

        for state in VALID_MODIFICATION_STATES {
            update_entity_conn(
                &mut conn,
                &id,
                &EntityUpdate {
                    modification_state: Some((*state).to_owned()),
                    ..Default::default()
                },
            )
            .await
            .unwrap_or_else(|e| panic!("modification_state '{state}' rejected: {e}"));
        }

        assert!(
            update_entity_conn(
                &mut conn,
                &id,
                &EntityUpdate {
                    modification_state: Some("invented".to_owned()),
                    ..Default::default()
                },
            )
            .await
            .is_err(),
            "an unknown modification_state must be rejected up front"
        );
    }

    // -- personal_003 migration (decisions.id=513, items.id=175) -----------

    async fn apply_v3(conn: &mut SqliteConnection) {
        for stmt in parse_statements(PERSONAL_SCHEMA_V3) {
            sqlx::query(&stmt)
                .execute(&mut *conn)
                .await
                .unwrap_or_else(|e| panic!("v3 statement failed: {e}\n{stmt}"));
        }
    }

    #[tokio::test]
    async fn migration_v3_adds_visibility_columns_defaulting_false_and_preserves_rows() {
        // Applies v1+v2 first (pre-existing row, no knowledge of the new
        // columns), then v3 -- confirms ADD COLUMN is additive: no row lost,
        // no pre-existing column disturbed, new columns default to false.
        let mut conn = SqliteConnectOptions::new()
            .filename(":memory:")
            .connect()
            .await
            .expect("in-memory connection failed");
        for schema in [PERSONAL_SCHEMA_V1, PERSONAL_SCHEMA_V2] {
            for stmt in parse_statements(schema) {
                sqlx::query(&stmt).execute(&mut conn).await.unwrap();
            }
        }

        let id = create_entity_conn(&mut conn, "recipe", "Pre-v3 Bread", &[], None, None)
            .await
            .expect("pre-v3 create failed");

        apply_v3(&mut conn).await;

        let e = get_entity_conn(&mut conn, &id)
            .await
            .expect("get failed")
            .expect("row must survive the v3 ADD COLUMN migration");
        assert_eq!(e.display_name, "Pre-v3 Bread", "pre-existing data must survive");
        assert!(
            !e.redact_identification,
            "pre-existing rows must default to false, not NULL or an error"
        );
        assert!(!e.hide_from_shared_surfaces);
    }

    #[tokio::test]
    async fn migration_v3_check_constraints_reject_non_boolean_values() {
        let mut conn = SqliteConnectOptions::new()
            .filename(":memory:")
            .connect()
            .await
            .expect("in-memory connection failed");
        for schema in [PERSONAL_SCHEMA_V1, PERSONAL_SCHEMA_V2, PERSONAL_SCHEMA_V3] {
            for stmt in parse_statements(schema) {
                sqlx::query(&stmt).execute(&mut conn).await.unwrap();
            }
        }

        let id = create_entity_conn(&mut conn, "recipe", "Bread", &[], None, None)
            .await
            .unwrap();

        let result = sqlx::query("UPDATE entities SET redact_identification = 2 WHERE id = ?")
            .bind(&id)
            .execute(&mut conn)
            .await;
        assert!(
            result.is_err(),
            "the CHECK (redact_identification IN (0, 1)) constraint must reject 2"
        );
    }
}
