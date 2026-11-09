// src-tauri/src/persistence/case_store.rs
//
// items.id=881 -- typed store for case_records, case_entities,
// case_strategies, config_deltas and config_delta_entities
// (schema/outputs_013.sql, decisions.id=508, 510, 939) in the persona's
// outputs.db. Typed API only: nothing here is wired into the executor.
// Seeding and the taint closure are items.id=886, the store_op writers are
// items.id=887, and entity-purge cleanup, retention and archive are
// items.id=888, so there is deliberately no function here that deletes a case,
// a link or a delta.
//
// CONNECTIONS AND CONCURRENCY: the same as session_store.rs. Every function
// takes a caller-supplied connection to the persona's outputs.db, the persona
// is the file, and every write runs in one `BEGIN IMMEDIATE` transaction.
//
// THE LATCH: every write that happens inside a chat takes that chat's
// `context_key` and is refused, inside its own transaction, once the chat is
// latched write_mode = suppressed, or if the latch cannot be read
// (session_store::ensure_writable, fail closed). The one exception is
// refresh_subject_label: it is driven by the device library's rename, which
// has no chat, and a one-time session creates no entity and no case for it to
// touch (decisions.id=509), so there is no chat latch to apply to it.
//
// APPEND-ONLY: case_strategies is the strategies_attempted log of
// decisions.id=508. This module has no function that updates or deletes a
// strategy row, and a test scans the source tree to keep it that way. An
// append of a step attempt that is already recorded is a no-op that returns
// the existing row and never overwrites it.
//
// OPTIMISTIC CONCURRENCY: case_records.version is bumped by every update_case
// and checked on write, so two passes updating one case cannot silently
// overwrite each other. refresh_subject_label changes only the display label
// and does not bump it.
//
// DEGRADATION IS COMPUTED AT READ TIME (decisions.id=510): the status stored
// in config_delta_entities is what was last recorded. effective_status turns a
// stored believed-active or confirmed-active older than the degradation window
// into unknown when a delta is read, and nothing is written back.
//
// QUERY STYLE: runtime sqlx::query() only, never the query!() macros.

use chrono::{DateTime, Duration, Utc};
use sqlx::{Row, SqliteConnection};
use thiserror::Error;

use crate::persistence::session_store::{
    begin_write, ensure_writable, finish_write, validate_context_key, validate_text, LatchError,
};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Days after which a stored believed-active or confirmed-active config delta
/// status reads as unknown (decisions.id=510's default; the per-entity
/// override that 510 mentions is not built, effective_status takes the window
/// as an argument).
pub const CONFIG_DELTA_DEGRADE_DAYS: i64 = 90;

pub const CASE_TEXT_MAX_BYTES: usize = 4096;
pub const SUBJECT_LABEL_MAX_BYTES: usize = 256;
pub const DURABLE_FACTS_MAX_BYTES: usize = 8192;
pub const STRATEGY_TEXT_MAX_BYTES: usize = 2048;
pub const DELTA_DESCRIPTION_MAX_BYTES: usize = 2048;
pub const RELEVANCE_TAGS_MAX_BYTES: usize = 1024;

const ID_MAX_CHARS: usize = 128;
const RUN_MAX_CHARS: usize = 256;

/// The four non-terminal lifecycle states, as a SQL list. It repeats the
/// predicate of idx_case_records_open so the partial index is usable.
const OPEN_STATES_SQL: &str = "('open','in-progress','escalated','awaiting-external')";

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum CaseStoreError {
    #[error("Database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("{0}")]
    Validation(String),
    #[error("not found: {0}")]
    NotFound(String),
    /// The case is in a terminal state and takes no further writes.
    #[error("case '{0}' is closed")]
    CaseClosed(String),
    /// The case id exists but belongs to a different primary subject.
    #[error("case id '{0}' is already used for a different subject")]
    CaseIdConflict(String),
    #[error("case version conflict (current version {current_version})")]
    VersionConflict { current_version: i64 },
    #[error("{0}")]
    Latch(#[from] LatchError),
}

fn invalid(msg: impl Into<String>) -> CaseStoreError {
    CaseStoreError::Validation(msg.into())
}

// ---------------------------------------------------------------------------
// Enums (names exactly as decisions.id=508 and 510)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaseState {
    Open,
    InProgress,
    Escalated,
    AwaitingExternal,
    Resolved,
    Replaced,
    Closed,
}

impl CaseState {
    pub fn as_str(self) -> &'static str {
        match self {
            CaseState::Open => "open",
            CaseState::InProgress => "in-progress",
            CaseState::Escalated => "escalated",
            CaseState::AwaitingExternal => "awaiting-external",
            CaseState::Resolved => "resolved",
            CaseState::Replaced => "replaced",
            CaseState::Closed => "closed",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "open" => CaseState::Open,
            "in-progress" => CaseState::InProgress,
            "escalated" => CaseState::Escalated,
            "awaiting-external" => CaseState::AwaitingExternal,
            "resolved" => CaseState::Resolved,
            "replaced" => CaseState::Replaced,
            "closed" => CaseState::Closed,
            _ => return None,
        })
    }

    /// resolved, replaced and closed: the case has a closed_at and takes no
    /// further writes.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            CaseState::Resolved | CaseState::Replaced | CaseState::Closed
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolutionPath {
    Resolved,
    EscalatedHuman,
    WarrantyRma,
    Replaced,
}

impl ResolutionPath {
    pub fn as_str(self) -> &'static str {
        match self {
            ResolutionPath::Resolved => "resolved",
            ResolutionPath::EscalatedHuman => "escalated-human",
            ResolutionPath::WarrantyRma => "warranty-rma",
            ResolutionPath::Replaced => "replaced",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "resolved" => ResolutionPath::Resolved,
            "escalated-human" => ResolutionPath::EscalatedHuman,
            "warranty-rma" => ResolutionPath::WarrantyRma,
            "replaced" => ResolutionPath::Replaced,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntityRole {
    Primary,
    Affected,
}

impl EntityRole {
    pub fn as_str(self) -> &'static str {
        match self {
            EntityRole::Primary => "primary",
            EntityRole::Affected => "affected",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s {
            "primary" => Some(EntityRole::Primary),
            "affected" => Some(EntityRole::Affected),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeltaStatus {
    BelievedActive,
    ConfirmedActive,
    BelievedChanged,
    Unknown,
}

impl DeltaStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            DeltaStatus::BelievedActive => "believed-active",
            DeltaStatus::ConfirmedActive => "confirmed-active",
            DeltaStatus::BelievedChanged => "believed-changed",
            DeltaStatus::Unknown => "unknown",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "believed-active" => DeltaStatus::BelievedActive,
            "confirmed-active" => DeltaStatus::ConfirmedActive,
            "believed-changed" => DeltaStatus::BelievedChanged,
            "unknown" => DeltaStatus::Unknown,
            _ => return None,
        })
    }
}

// ---------------------------------------------------------------------------
// Row types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaseRecord {
    pub case_id: String,
    pub state: CaseState,
    pub subject_label: String,
    pub symptoms: String,
    pub current_rung: i64,
    pub resolution_path: Option<ResolutionPath>,
    pub outcome: Option<String>,
    /// JSON text, `[]` until set.
    pub durable_facts_extracted: String,
    pub version: i64,
    pub opened_by_run: String,
    pub opened_at: String,
    pub closed_at: Option<String>,
    pub updated_at: String,
    /// The case's one role-primary entity link.
    pub primary_entity_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaseEntityLink {
    pub entity_id: String,
    pub role: EntityRole,
    pub linked_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaseStrategy {
    pub strategy_id: i64,
    pub case_id: String,
    pub source_run_id: String,
    pub source_action_id: String,
    pub source_attempt: i64,
    pub strategy: String,
    pub outcome: String,
    pub user_signal: Option<String>,
    pub recorded_at: String,
}

/// A config delta as seen for one entity, with the status as stored and as it
/// reads at `now`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntityConfigDelta {
    pub delta_id: String,
    pub session_reference: String,
    pub description: String,
    pub relevance_tags: Vec<String>,
    pub changed_at: String,
    pub entity_id: String,
    pub stored_status: DeltaStatus,
    pub status_changed_at: String,
    pub effective_status: DeltaStatus,
}

// ---------------------------------------------------------------------------
// Inputs and outcomes
// ---------------------------------------------------------------------------

/// `case_id` is the idempotency key and is chosen by the caller: opening the
/// same id again for the same primary subject returns the existing case.
#[derive(Debug, Clone)]
pub struct NewCase<'a> {
    pub case_id: &'a str,
    pub context_key: &'a str,
    pub primary_entity_id: &'a str,
    pub subject_label: &'a str,
    pub symptoms: &'a str,
    pub opened_by_run: &'a str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenOutcome {
    Created(CaseRecord),
    Existing(CaseRecord),
}

impl OpenOutcome {
    pub fn record(&self) -> &CaseRecord {
        match self {
            OpenOutcome::Created(r) | OpenOutcome::Existing(r) => r,
        }
    }
}

/// Fields to change. None leaves a field as it is. At least one is required.
#[derive(Debug, Clone, Default)]
pub struct CaseUpdate<'a> {
    pub state: Option<CaseState>,
    pub current_rung: Option<i64>,
    pub resolution_path: Option<ResolutionPath>,
    pub symptoms: Option<&'a str>,
    pub outcome: Option<&'a str>,
    /// JSON text.
    pub durable_facts_json: Option<&'a str>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkOutcome {
    Linked,
    AlreadyLinked,
}

/// The idempotency key is (case_id, source_run_id, source_action_id,
/// source_attempt).
#[derive(Debug, Clone)]
pub struct NewStrategy<'a> {
    pub case_id: &'a str,
    pub source_run_id: &'a str,
    pub source_action_id: &'a str,
    pub source_attempt: i64,
    pub strategy: &'a str,
    pub outcome: &'a str,
    pub user_signal: Option<&'a str>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppendOutcome {
    Appended {
        strategy_id: i64,
    },
    /// This step attempt was already recorded. Nothing was written and the
    /// existing row is untouched.
    Duplicate {
        strategy_id: i64,
    },
}

/// `delta_id` is the idempotency key, chosen by the caller.
#[derive(Debug, Clone)]
pub struct NewConfigDelta<'a> {
    pub delta_id: &'a str,
    pub context_key: &'a str,
    /// The session the change was made in.
    pub session_reference: &'a str,
    pub description: &'a str,
    pub relevance_tags: &'a [&'a str],
    /// At least one. Each starts believed-active.
    pub entity_ids: &'a [&'a str],
    /// RFC 3339. None means now.
    pub changed_at: Option<&'a str>,
    pub recorded_by_run: &'a str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeltaOutcome {
    Created,
    /// The delta id was already recorded. Nothing was written.
    Existing,
}

// ---------------------------------------------------------------------------
// Validation helpers
// ---------------------------------------------------------------------------

fn check_bytes(label: &str, s: &str, max: usize) -> Result<(), CaseStoreError> {
    if s.len() > max {
        return Err(invalid(format!(
            "{label} is {} bytes, the limit is {max}",
            s.len()
        )));
    }
    if s.contains('\0') {
        return Err(invalid(format!("{label} must not contain a NUL character")));
    }
    Ok(())
}

fn check_id(label: &str, s: &str) -> Result<(), CaseStoreError> {
    validate_text(label, s, ID_MAX_CHARS).map_err(CaseStoreError::Validation)
}

fn check_run(label: &str, s: &str) -> Result<(), CaseStoreError> {
    validate_text(label, s, RUN_MAX_CHARS).map_err(CaseStoreError::Validation)
}

fn check_context(s: &str) -> Result<(), CaseStoreError> {
    validate_context_key(s).map_err(CaseStoreError::Validation)
}

fn check_json(label: &str, s: &str, max: usize) -> Result<(), CaseStoreError> {
    check_bytes(label, s, max)?;
    serde_json::from_str::<serde_json::Value>(s)
        .map(|_| ())
        .map_err(|e| invalid(format!("{label} is not valid JSON: {e}")))
}

fn decode_err(msg: String) -> sqlx::Error {
    sqlx::Error::Decode(msg.into())
}

// ---------------------------------------------------------------------------
// Row mapping
// ---------------------------------------------------------------------------

const CASE_SELECT: &str = "SELECT c.case_id, c.lifecycle_state, c.subject_label, c.symptoms,
        c.current_rung, c.resolution_path, c.outcome, c.durable_facts_extracted,
        c.version, c.opened_by_run, c.opened_at, c.closed_at, c.updated_at,
        (SELECT e.entity_id FROM case_entities e
          WHERE e.case_id = c.case_id AND e.role = 'primary') AS primary_entity_id
     FROM case_records c";

fn row_to_case(r: &sqlx::sqlite::SqliteRow) -> Result<CaseRecord, sqlx::Error> {
    let state: String = r.try_get("lifecycle_state")?;
    let path: Option<String> = r.try_get("resolution_path")?;
    Ok(CaseRecord {
        case_id: r.try_get("case_id")?,
        state: CaseState::parse(&state)
            .ok_or_else(|| decode_err(format!("unknown lifecycle_state '{state}'")))?,
        subject_label: r.try_get("subject_label")?,
        symptoms: r.try_get("symptoms")?,
        current_rung: r.try_get("current_rung")?,
        resolution_path: match path {
            None => None,
            Some(p) => Some(
                ResolutionPath::parse(&p)
                    .ok_or_else(|| decode_err(format!("unknown resolution_path '{p}'")))?,
            ),
        },
        outcome: r.try_get("outcome")?,
        durable_facts_extracted: r.try_get("durable_facts_extracted")?,
        version: r.try_get("version")?,
        opened_by_run: r.try_get("opened_by_run")?,
        opened_at: r.try_get("opened_at")?,
        closed_at: r.try_get("closed_at")?,
        updated_at: r.try_get("updated_at")?,
        primary_entity_id: r.try_get("primary_entity_id")?,
    })
}

fn row_to_strategy(r: &sqlx::sqlite::SqliteRow) -> Result<CaseStrategy, sqlx::Error> {
    Ok(CaseStrategy {
        strategy_id: r.try_get("strategy_id")?,
        case_id: r.try_get("case_id")?,
        source_run_id: r.try_get("source_run_id")?,
        source_action_id: r.try_get("source_action_id")?,
        source_attempt: r.try_get("source_attempt")?,
        strategy: r.try_get("strategy")?,
        outcome: r.try_get("outcome")?,
        user_signal: r.try_get("user_signal")?,
        recorded_at: r.try_get("recorded_at")?,
    })
}

async fn fetch_case(
    conn: &mut SqliteConnection,
    case_id: &str,
) -> Result<Option<CaseRecord>, sqlx::Error> {
    let row = sqlx::query(&format!("{CASE_SELECT} WHERE c.case_id = ?"))
        .bind(case_id)
        .fetch_optional(&mut *conn)
        .await?;
    row.as_ref().map(row_to_case).transpose()
}

// ---------------------------------------------------------------------------
// Case records
// ---------------------------------------------------------------------------

/// Open a case for a primary subject, idempotent on `case_id`. A case always
/// has one primary entity link (a case with none is purged by items.id=888).
/// Starts in state open at rung 1. Refused once the chat is latched.
pub async fn open_case(
    conn: &mut SqliteConnection,
    new: &NewCase<'_>,
) -> Result<OpenOutcome, CaseStoreError> {
    check_context(new.context_key)?;
    check_id("case_id", new.case_id)?;
    check_id("primary_entity_id", new.primary_entity_id)?;
    check_run("opened_by_run", new.opened_by_run)?;
    check_bytes("subject_label", new.subject_label, SUBJECT_LABEL_MAX_BYTES)?;
    check_bytes("symptoms", new.symptoms, CASE_TEXT_MAX_BYTES)?;

    begin_write(conn).await?;
    let result = open_case_inner(conn, new).await;
    finish_write(conn, result).await
}

async fn open_case_inner(
    conn: &mut SqliteConnection,
    new: &NewCase<'_>,
) -> Result<OpenOutcome, CaseStoreError> {
    ensure_writable(conn, new.context_key).await?;

    if let Some(existing) = fetch_case(conn, new.case_id).await? {
        return if existing.primary_entity_id.as_deref() == Some(new.primary_entity_id) {
            Ok(OpenOutcome::Existing(existing))
        } else {
            Err(CaseStoreError::CaseIdConflict(new.case_id.to_owned()))
        };
    }

    let now = crate::providers::utils::now();
    sqlx::query(
        "INSERT INTO case_records
         (case_id, lifecycle_state, subject_label, symptoms, current_rung,
          version, opened_by_run, opened_at, updated_at)
         VALUES (?, 'open', ?, ?, 1, 1, ?, ?, ?)",
    )
    .bind(new.case_id)
    .bind(new.subject_label)
    .bind(new.symptoms)
    .bind(new.opened_by_run)
    .bind(&now)
    .bind(&now)
    .execute(&mut *conn)
    .await?;
    sqlx::query(
        "INSERT INTO case_entities (case_id, entity_id, role, linked_at)
         VALUES (?, ?, 'primary', ?)",
    )
    .bind(new.case_id)
    .bind(new.primary_entity_id)
    .bind(&now)
    .execute(&mut *conn)
    .await?;

    let created = fetch_case(conn, new.case_id)
        .await?
        .ok_or_else(|| CaseStoreError::NotFound(new.case_id.to_owned()))?;
    Ok(OpenOutcome::Created(created))
}

pub async fn get_case(
    conn: &mut SqliteConnection,
    case_id: &str,
) -> Result<Option<CaseRecord>, CaseStoreError> {
    check_id("case_id", case_id)?;
    Ok(fetch_case(conn, case_id).await?)
}

/// Open cases (any non-terminal state) linked to an entity, oldest first.
/// `role` narrows to the case's primary subject or to an affected entity, None
/// takes either.
pub async fn list_open_cases_for_entity(
    conn: &mut SqliteConnection,
    entity_id: &str,
    role: Option<EntityRole>,
) -> Result<Vec<CaseRecord>, CaseStoreError> {
    check_id("entity_id", entity_id)?;
    let role = role.map(EntityRole::as_str);
    let rows = sqlx::query(&format!(
        "{CASE_SELECT}
         WHERE c.lifecycle_state IN {OPEN_STATES_SQL}
           AND EXISTS (SELECT 1 FROM case_entities l
                        WHERE l.case_id = c.case_id AND l.entity_id = ?
                          AND (? IS NULL OR l.role = ?))
         ORDER BY c.opened_at, c.case_id"
    ))
    .bind(entity_id)
    .bind(role)
    .bind(role)
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows
        .iter()
        .map(row_to_case)
        .collect::<Result<Vec<_>, _>>()?)
}

/// Update a case if its version is still `expected_version`. Bumps the version
/// and returns the new record. A terminal case takes no updates. A state that
/// is terminal sets closed_at. Refused once the chat is latched.
pub async fn update_case(
    conn: &mut SqliteConnection,
    context_key: &str,
    case_id: &str,
    expected_version: i64,
    update: &CaseUpdate<'_>,
) -> Result<CaseRecord, CaseStoreError> {
    check_context(context_key)?;
    check_id("case_id", case_id)?;
    if expected_version < 1 {
        return Err(invalid("expected_version must be at least 1"));
    }
    if update.state.is_none()
        && update.current_rung.is_none()
        && update.resolution_path.is_none()
        && update.symptoms.is_none()
        && update.outcome.is_none()
        && update.durable_facts_json.is_none()
    {
        return Err(invalid("an update must change at least one field"));
    }
    if let Some(rung) = update.current_rung {
        if !(1..=5).contains(&rung) {
            return Err(invalid("current_rung must be between 1 and 5"));
        }
    }
    if let Some(s) = update.symptoms {
        check_bytes("symptoms", s, CASE_TEXT_MAX_BYTES)?;
    }
    if let Some(s) = update.outcome {
        check_bytes("outcome", s, CASE_TEXT_MAX_BYTES)?;
    }
    if let Some(s) = update.durable_facts_json {
        check_json("durable_facts_json", s, DURABLE_FACTS_MAX_BYTES)?;
    }

    begin_write(conn).await?;
    let result = update_case_inner(conn, context_key, case_id, expected_version, update).await;
    finish_write(conn, result).await
}

async fn update_case_inner(
    conn: &mut SqliteConnection,
    context_key: &str,
    case_id: &str,
    expected_version: i64,
    update: &CaseUpdate<'_>,
) -> Result<CaseRecord, CaseStoreError> {
    ensure_writable(conn, context_key).await?;

    let current = fetch_case(conn, case_id)
        .await?
        .ok_or_else(|| CaseStoreError::NotFound(case_id.to_owned()))?;
    if current.state.is_terminal() {
        return Err(CaseStoreError::CaseClosed(case_id.to_owned()));
    }
    if current.version != expected_version {
        return Err(CaseStoreError::VersionConflict {
            current_version: current.version,
        });
    }

    let now = crate::providers::utils::now();
    let state = update.state.unwrap_or(current.state);
    let rung = update.current_rung.unwrap_or(current.current_rung);
    let path = update.resolution_path.or(current.resolution_path);
    let symptoms = update.symptoms.unwrap_or(&current.symptoms);
    let outcome = match update.outcome {
        Some(o) => Some(o.to_owned()),
        None => current.outcome.clone(),
    };
    let facts = update
        .durable_facts_json
        .unwrap_or(&current.durable_facts_extracted);
    let closed_at = state.is_terminal().then(|| now.clone());

    let done = sqlx::query(
        "UPDATE case_records
         SET lifecycle_state = ?, current_rung = ?, resolution_path = ?, symptoms = ?,
             outcome = ?, durable_facts_extracted = ?, closed_at = ?,
             version = version + 1, updated_at = ?
         WHERE case_id = ? AND version = ?",
    )
    .bind(state.as_str())
    .bind(rung)
    .bind(path.map(ResolutionPath::as_str))
    .bind(symptoms)
    .bind(outcome)
    .bind(facts)
    .bind(closed_at)
    .bind(&now)
    .bind(case_id)
    .bind(expected_version)
    .execute(&mut *conn)
    .await?;
    if done.rows_affected() != 1 {
        return Err(CaseStoreError::VersionConflict {
            current_version: current.version,
        });
    }
    fetch_case(conn, case_id)
        .await?
        .ok_or_else(|| CaseStoreError::NotFound(case_id.to_owned()))
}

/// Refresh the display label of every case whose primary subject is
/// `entity_id` (the device library's rename path, decisions.id=939 section 7).
/// Returns how many cases changed. See the module header for why this one
/// write has no chat latch.
pub async fn refresh_subject_label(
    conn: &mut SqliteConnection,
    entity_id: &str,
    new_label: &str,
) -> Result<u64, CaseStoreError> {
    check_id("entity_id", entity_id)?;
    check_bytes("subject_label", new_label, SUBJECT_LABEL_MAX_BYTES)?;
    let done = sqlx::query(
        "UPDATE case_records
         SET subject_label = ?, updated_at = ?
         WHERE case_id IN (SELECT case_id FROM case_entities
                            WHERE entity_id = ? AND role = 'primary')
           AND subject_label <> ?",
    )
    .bind(new_label)
    .bind(crate::providers::utils::now())
    .bind(entity_id)
    .bind(new_label)
    .execute(&mut *conn)
    .await?;
    Ok(done.rows_affected())
}

// ---------------------------------------------------------------------------
// Case entities
// ---------------------------------------------------------------------------

/// Link an affected entity to an open case. Idempotent: linking an entity the
/// case already has (in any role) is AlreadyLinked. Refused for a closed case
/// and once the chat is latched.
pub async fn link_case_entity(
    conn: &mut SqliteConnection,
    context_key: &str,
    case_id: &str,
    entity_id: &str,
) -> Result<LinkOutcome, CaseStoreError> {
    check_context(context_key)?;
    check_id("case_id", case_id)?;
    check_id("entity_id", entity_id)?;

    begin_write(conn).await?;
    let result = link_inner(conn, context_key, case_id, entity_id).await;
    finish_write(conn, result).await
}

async fn link_inner(
    conn: &mut SqliteConnection,
    context_key: &str,
    case_id: &str,
    entity_id: &str,
) -> Result<LinkOutcome, CaseStoreError> {
    ensure_writable(conn, context_key).await?;
    let case = fetch_case(conn, case_id)
        .await?
        .ok_or_else(|| CaseStoreError::NotFound(case_id.to_owned()))?;
    let linked: Option<i64> =
        sqlx::query_scalar("SELECT 1 FROM case_entities WHERE case_id = ? AND entity_id = ?")
            .bind(case_id)
            .bind(entity_id)
            .fetch_optional(&mut *conn)
            .await?;
    if linked.is_some() {
        return Ok(LinkOutcome::AlreadyLinked);
    }
    if case.state.is_terminal() {
        return Err(CaseStoreError::CaseClosed(case_id.to_owned()));
    }
    sqlx::query(
        "INSERT INTO case_entities (case_id, entity_id, role, linked_at)
         VALUES (?, ?, 'affected', ?)",
    )
    .bind(case_id)
    .bind(entity_id)
    .bind(crate::providers::utils::now())
    .execute(&mut *conn)
    .await?;
    Ok(LinkOutcome::Linked)
}

pub async fn list_case_entities(
    conn: &mut SqliteConnection,
    case_id: &str,
) -> Result<Vec<CaseEntityLink>, CaseStoreError> {
    check_id("case_id", case_id)?;
    let rows = sqlx::query(
        "SELECT entity_id, role, linked_at FROM case_entities
         WHERE case_id = ? ORDER BY role, entity_id",
    )
    .bind(case_id)
    .fetch_all(&mut *conn)
    .await?;
    rows.iter()
        .map(|r| {
            let role: String = r.try_get("role")?;
            Ok(CaseEntityLink {
                entity_id: r.try_get("entity_id")?,
                role: EntityRole::parse(&role)
                    .ok_or_else(|| decode_err(format!("unknown role '{role}'")))?,
                linked_at: r.try_get("linked_at")?,
            })
        })
        .collect::<Result<Vec<_>, sqlx::Error>>()
        .map_err(Into::into)
}

// ---------------------------------------------------------------------------
// Strategies (append-only)
// ---------------------------------------------------------------------------

/// Append one strategy to a case's log, idempotent on (case, run, action,
/// attempt). A step attempt that is already recorded returns Duplicate and is
/// never overwritten. Refused for a closed case (unless it is a duplicate) and
/// once the chat is latched. There is no update or delete counterpart.
pub async fn append_case_strategy(
    conn: &mut SqliteConnection,
    context_key: &str,
    new: &NewStrategy<'_>,
) -> Result<AppendOutcome, CaseStoreError> {
    check_context(context_key)?;
    check_id("case_id", new.case_id)?;
    check_run("source_run_id", new.source_run_id)?;
    check_id("source_action_id", new.source_action_id)?;
    if new.source_attempt < 0 {
        return Err(invalid("source_attempt must not be negative"));
    }
    if new.strategy.is_empty() || new.outcome.is_empty() {
        return Err(invalid("strategy and outcome must not be empty"));
    }
    check_bytes("strategy", new.strategy, STRATEGY_TEXT_MAX_BYTES)?;
    check_bytes("outcome", new.outcome, STRATEGY_TEXT_MAX_BYTES)?;
    if let Some(s) = new.user_signal {
        check_bytes("user_signal", s, STRATEGY_TEXT_MAX_BYTES)?;
    }

    begin_write(conn).await?;
    let result = append_inner(conn, context_key, new).await;
    finish_write(conn, result).await
}

async fn append_inner(
    conn: &mut SqliteConnection,
    context_key: &str,
    new: &NewStrategy<'_>,
) -> Result<AppendOutcome, CaseStoreError> {
    ensure_writable(conn, context_key).await?;

    let existing: Option<i64> = sqlx::query_scalar(
        "SELECT strategy_id FROM case_strategies
         WHERE case_id = ? AND source_run_id = ? AND source_action_id = ? AND source_attempt = ?",
    )
    .bind(new.case_id)
    .bind(new.source_run_id)
    .bind(new.source_action_id)
    .bind(new.source_attempt)
    .fetch_optional(&mut *conn)
    .await?;
    if let Some(strategy_id) = existing {
        return Ok(AppendOutcome::Duplicate { strategy_id });
    }

    let case = fetch_case(conn, new.case_id)
        .await?
        .ok_or_else(|| CaseStoreError::NotFound(new.case_id.to_owned()))?;
    if case.state.is_terminal() {
        return Err(CaseStoreError::CaseClosed(new.case_id.to_owned()));
    }

    let done = sqlx::query(
        "INSERT INTO case_strategies
         (case_id, source_run_id, source_action_id, source_attempt,
          strategy, outcome, user_signal, recorded_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(new.case_id)
    .bind(new.source_run_id)
    .bind(new.source_action_id)
    .bind(new.source_attempt)
    .bind(new.strategy)
    .bind(new.outcome)
    .bind(new.user_signal)
    .bind(crate::providers::utils::now())
    .execute(&mut *conn)
    .await?;
    Ok(AppendOutcome::Appended {
        strategy_id: done.last_insert_rowid(),
    })
}

/// A case's strategies in the order they were appended.
pub async fn list_case_strategies(
    conn: &mut SqliteConnection,
    case_id: &str,
) -> Result<Vec<CaseStrategy>, CaseStoreError> {
    check_id("case_id", case_id)?;
    let rows = sqlx::query(
        "SELECT strategy_id, case_id, source_run_id, source_action_id, source_attempt,
                strategy, outcome, user_signal, recorded_at
         FROM case_strategies WHERE case_id = ? ORDER BY strategy_id",
    )
    .bind(case_id)
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows
        .iter()
        .map(row_to_strategy)
        .collect::<Result<Vec<_>, _>>()?)
}

// ---------------------------------------------------------------------------
// Config deltas
// ---------------------------------------------------------------------------

/// Record a configuration change and the entities it affected, idempotent on
/// `delta_id`. Every entity starts believed-active, with the clock at
/// `changed_at`. Refused once the chat is latched.
pub async fn record_config_delta(
    conn: &mut SqliteConnection,
    new: &NewConfigDelta<'_>,
) -> Result<DeltaOutcome, CaseStoreError> {
    check_context(new.context_key)?;
    check_id("delta_id", new.delta_id)?;
    check_run("session_reference", new.session_reference)?;
    check_run("recorded_by_run", new.recorded_by_run)?;
    if new.description.is_empty() {
        return Err(invalid("description must not be empty"));
    }
    check_bytes("description", new.description, DELTA_DESCRIPTION_MAX_BYTES)?;
    if new.entity_ids.is_empty() {
        return Err(invalid("a config delta must name at least one entity"));
    }
    let mut entity_ids: Vec<&str> = Vec::with_capacity(new.entity_ids.len());
    for id in new.entity_ids {
        check_id("entity_id", id)?;
        if !entity_ids.contains(id) {
            entity_ids.push(id);
        }
    }
    let tags = serde_json::to_string(new.relevance_tags)
        .map_err(|e| invalid(format!("relevance_tags: {e}")))?;
    check_bytes("relevance_tags", &tags, RELEVANCE_TAGS_MAX_BYTES)?;
    let changed_at = match new.changed_at {
        Some(s) => {
            DateTime::parse_from_rfc3339(s)
                .map_err(|e| invalid(format!("changed_at is not RFC 3339: {e}")))?;
            s.to_owned()
        }
        None => crate::providers::utils::now(),
    };

    begin_write(conn).await?;
    let result = record_delta_inner(conn, new, &entity_ids, &tags, &changed_at).await;
    finish_write(conn, result).await
}

async fn record_delta_inner(
    conn: &mut SqliteConnection,
    new: &NewConfigDelta<'_>,
    entity_ids: &[&str],
    tags_json: &str,
    changed_at: &str,
) -> Result<DeltaOutcome, CaseStoreError> {
    ensure_writable(conn, new.context_key).await?;

    let now = crate::providers::utils::now();
    let done = sqlx::query(
        "INSERT INTO config_deltas
         (delta_id, session_reference, description, relevance_tags, changed_at,
          recorded_by_run, created_at)
         VALUES (?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT(delta_id) DO NOTHING",
    )
    .bind(new.delta_id)
    .bind(new.session_reference)
    .bind(new.description)
    .bind(tags_json)
    .bind(changed_at)
    .bind(new.recorded_by_run)
    .bind(&now)
    .execute(&mut *conn)
    .await?;
    if done.rows_affected() == 0 {
        return Ok(DeltaOutcome::Existing);
    }
    for entity_id in entity_ids {
        sqlx::query(
            "INSERT INTO config_delta_entities (delta_id, entity_id, status, status_changed_at)
             VALUES (?, ?, 'believed-active', ?)",
        )
        .bind(new.delta_id)
        .bind(entity_id)
        .bind(changed_at)
        .execute(&mut *conn)
        .await?;
    }
    Ok(DeltaOutcome::Created)
}

/// Record a new status for one entity of a delta and restart its clock at
/// `at` (RFC 3339, None means now). This is the confirmation (decisions.id=510
/// "confirmed-active promotion") and the event path (believed-changed).
/// Refused once the chat is latched.
pub async fn set_delta_entity_status(
    conn: &mut SqliteConnection,
    context_key: &str,
    delta_id: &str,
    entity_id: &str,
    status: DeltaStatus,
    at: Option<&str>,
) -> Result<(), CaseStoreError> {
    check_context(context_key)?;
    check_id("delta_id", delta_id)?;
    check_id("entity_id", entity_id)?;
    let at = match at {
        Some(s) => {
            DateTime::parse_from_rfc3339(s)
                .map_err(|e| invalid(format!("at is not RFC 3339: {e}")))?;
            s.to_owned()
        }
        None => crate::providers::utils::now(),
    };

    begin_write(conn).await?;
    let result = set_status_inner(conn, context_key, delta_id, entity_id, status, &at).await;
    finish_write(conn, result).await
}

async fn set_status_inner(
    conn: &mut SqliteConnection,
    context_key: &str,
    delta_id: &str,
    entity_id: &str,
    status: DeltaStatus,
    at: &str,
) -> Result<(), CaseStoreError> {
    ensure_writable(conn, context_key).await?;
    let done = sqlx::query(
        "UPDATE config_delta_entities SET status = ?, status_changed_at = ?
         WHERE delta_id = ? AND entity_id = ?",
    )
    .bind(status.as_str())
    .bind(at)
    .bind(delta_id)
    .bind(entity_id)
    .execute(&mut *conn)
    .await?;
    if done.rows_affected() == 0 {
        return Err(CaseStoreError::NotFound(format!(
            "{delta_id} / {entity_id}"
        )));
    }
    Ok(())
}

/// What a stored status reads as at `now` (decisions.id=510). A believed-active
/// or confirmed-active status whose clock is older than `degrade_after_days`
/// reads as unknown, and so does one whose timestamp cannot be parsed (the
/// cautious side). believed-changed and unknown never change on read. Exactly
/// `degrade_after_days` old is still in window.
///
/// INTERPRETATION TO CONFIRM: decisions.id=510 names believed-active as the
/// status that falls to unknown after 90 days, and also says confirmation
/// resets "the 90-day clock", which only means something if confirmed-active
/// is on the clock too. Both are degraded here. DEGRADING_STATUSES is the one
/// place to change that.
pub fn effective_status(
    stored: DeltaStatus,
    status_changed_at: &str,
    now: DateTime<Utc>,
    degrade_after_days: i64,
) -> DeltaStatus {
    const DEGRADING_STATUSES: [DeltaStatus; 2] =
        [DeltaStatus::BelievedActive, DeltaStatus::ConfirmedActive];
    if !DEGRADING_STATUSES.contains(&stored) {
        return stored;
    }
    match DateTime::parse_from_rfc3339(status_changed_at) {
        Ok(at) if now.signed_duration_since(at) <= Duration::days(degrade_after_days) => stored,
        _ => DeltaStatus::Unknown,
    }
}

/// Every config delta that names an entity, newest change first, each with its
/// status as stored and as it reads at `now`. Nothing is written back.
pub async fn list_config_deltas_for_entity(
    conn: &mut SqliteConnection,
    entity_id: &str,
    now: DateTime<Utc>,
) -> Result<Vec<EntityConfigDelta>, CaseStoreError> {
    check_id("entity_id", entity_id)?;
    let rows = sqlx::query(
        "SELECT d.delta_id, d.session_reference, d.description, d.relevance_tags,
                d.changed_at, e.entity_id, e.status, e.status_changed_at
         FROM config_delta_entities e
         JOIN config_deltas d ON d.delta_id = e.delta_id
         WHERE e.entity_id = ?
         ORDER BY d.changed_at DESC, d.delta_id",
    )
    .bind(entity_id)
    .fetch_all(&mut *conn)
    .await?;
    rows.iter()
        .map(|r| {
            let status: String = r.try_get("status")?;
            let stored = DeltaStatus::parse(&status)
                .ok_or_else(|| decode_err(format!("unknown status '{status}'")))?;
            let status_changed_at: String = r.try_get("status_changed_at")?;
            let tags: String = r.try_get("relevance_tags")?;
            Ok(EntityConfigDelta {
                delta_id: r.try_get("delta_id")?,
                session_reference: r.try_get("session_reference")?,
                description: r.try_get("description")?,
                relevance_tags: serde_json::from_str(&tags)
                    .map_err(|e| decode_err(format!("relevance_tags: {e}")))?,
                changed_at: r.try_get("changed_at")?,
                entity_id: r.try_get("entity_id")?,
                stored_status: stored,
                effective_status: effective_status(
                    stored,
                    &status_changed_at,
                    now,
                    CONFIG_DELTA_DEGRADE_DAYS,
                ),
                status_changed_at,
            })
        })
        .collect::<Result<Vec<_>, sqlx::Error>>()
        .map_err(Into::into)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persistence::session_store::test_db::mem_outputs_conn;
    use crate::persistence::session_store::{
        latch_write_suppressed, put_session_variable, SessionStoreError, SessionWrite,
    };
    use chrono::TimeZone;

    const CHAT: &str = "chat-1";
    const FOCUS: &str = "tech-support";
    const ROUTER: &str = "entity-router";
    const MODEM: &str = "entity-modem";

    fn new_case<'a>(case_id: &'a str, entity: &'a str) -> NewCase<'a> {
        NewCase {
            case_id,
            context_key: CHAT,
            primary_entity_id: entity,
            subject_label: "Living room router",
            symptoms: "drops every evening",
            opened_by_run: "run-a",
        }
    }

    async fn count(conn: &mut SqliteConnection, table: &str) -> i64 {
        sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {table}"))
            .fetch_one(&mut *conn)
            .await
            .unwrap()
    }

    fn strategy<'a>(
        case_id: &'a str,
        run: &'a str,
        action: &'a str,
        attempt: i64,
    ) -> NewStrategy<'a> {
        NewStrategy {
            case_id,
            source_run_id: run,
            source_action_id: action,
            source_attempt: attempt,
            strategy: "isolate the network layer",
            outcome: "device can ping the router, the issue is upstream",
            user_signal: None,
        }
    }

    // -- case records --------------------------------------------------------

    #[tokio::test]
    async fn open_case_starts_open_at_rung_one_with_one_primary_link() {
        let mut conn = mem_outputs_conn().await;
        let out = open_case(&mut conn, &new_case("case-1", ROUTER))
            .await
            .unwrap();
        let OpenOutcome::Created(case) = out else {
            panic!("expected Created")
        };
        assert_eq!(case.state, CaseState::Open);
        assert_eq!((case.current_rung, case.version), (1, 1));
        assert_eq!(case.primary_entity_id.as_deref(), Some(ROUTER));
        assert!(case.closed_at.is_none() && case.resolution_path.is_none());
        assert_eq!(case.durable_facts_extracted, "[]");

        let links = list_case_entities(&mut conn, "case-1").await.unwrap();
        assert_eq!(links.len(), 1);
        assert_eq!(
            (links[0].entity_id.as_str(), links[0].role),
            (ROUTER, EntityRole::Primary)
        );
    }

    #[tokio::test]
    async fn open_case_is_idempotent_on_the_case_id() {
        let mut conn = mem_outputs_conn().await;
        open_case(&mut conn, &new_case("case-1", ROUTER))
            .await
            .unwrap();
        let again = open_case(&mut conn, &new_case("case-1", ROUTER))
            .await
            .unwrap();
        assert!(matches!(again, OpenOutcome::Existing(_)));
        assert_eq!(count(&mut conn, "case_records").await, 1);
        assert_eq!(count(&mut conn, "case_entities").await, 1);

        let err = open_case(&mut conn, &new_case("case-1", MODEM))
            .await
            .unwrap_err();
        assert!(matches!(err, CaseStoreError::CaseIdConflict(_)), "{err:?}");
        assert_eq!(count(&mut conn, "case_records").await, 1);
    }

    #[tokio::test]
    async fn lifecycle_state_names_are_exactly_those_of_decision_508() {
        let names = [
            "open",
            "in-progress",
            "escalated",
            "awaiting-external",
            "resolved",
            "replaced",
            "closed",
        ];
        for n in names {
            let state = CaseState::parse(n).unwrap_or_else(|| panic!("{n} must parse"));
            assert_eq!(state.as_str(), n);
        }
        assert!(CaseState::parse("on-hold").is_none());

        // The schema accepts exactly these seven and rejects anything else.
        let mut conn = mem_outputs_conn().await;
        for (i, n) in names.iter().enumerate() {
            let terminal = CaseState::parse(n).unwrap().is_terminal();
            let closed = if terminal { "'t'" } else { "NULL" };
            let sql = format!(
                "INSERT INTO case_records (case_id, lifecycle_state, opened_by_run, opened_at,
                  closed_at, updated_at) VALUES ('c{i}', '{n}', 'r', 't', {closed}, 't')"
            );
            sqlx::query(&sql)
                .execute(&mut conn)
                .await
                .unwrap_or_else(|e| panic!("{n}: {e}"));
        }
        let bad = sqlx::query(
            "INSERT INTO case_records (case_id, lifecycle_state, opened_by_run, opened_at, updated_at)
             VALUES ('bad', 'in_progress', 'r', 't', 't')",
        )
        .execute(&mut conn)
        .await;
        assert!(bad.is_err());
    }

    #[tokio::test]
    async fn closed_at_is_set_exactly_when_the_state_is_terminal() {
        let mut conn = mem_outputs_conn().await;
        // A terminal state without closed_at, and an open state with one, are both refused.
        for sql in [
            "INSERT INTO case_records (case_id, lifecycle_state, opened_by_run, opened_at, updated_at)
             VALUES ('x1', 'closed', 'r', 't', 't')",
            "INSERT INTO case_records (case_id, lifecycle_state, opened_by_run, opened_at, closed_at, updated_at)
             VALUES ('x2', 'open', 'r', 't', 't', 't')",
        ] {
            assert!(sqlx::query(sql).execute(&mut conn).await.is_err(), "{sql}");
        }
    }

    #[tokio::test]
    async fn update_case_bumps_the_version_and_enforces_compare_and_set() {
        let mut conn = mem_outputs_conn().await;
        open_case(&mut conn, &new_case("case-1", ROUTER))
            .await
            .unwrap();

        let up = CaseUpdate {
            state: Some(CaseState::InProgress),
            current_rung: Some(2),
            ..Default::default()
        };
        let case = update_case(&mut conn, CHAT, "case-1", 1, &up)
            .await
            .unwrap();
        assert_eq!(
            (case.state, case.current_rung, case.version),
            (CaseState::InProgress, 2, 2)
        );
        assert!(case.closed_at.is_none());

        // A writer still holding version 1 loses and changes nothing.
        let stale = CaseUpdate {
            current_rung: Some(4),
            ..Default::default()
        };
        let err = update_case(&mut conn, CHAT, "case-1", 1, &stale)
            .await
            .unwrap_err();
        assert!(
            matches!(err, CaseStoreError::VersionConflict { current_version: 2 }),
            "{err:?}"
        );
        let kept = get_case(&mut conn, "case-1").await.unwrap().unwrap();
        assert_eq!((kept.current_rung, kept.version), (2, 2));

        // Unset fields stay as they were.
        let only_symptoms = CaseUpdate {
            symptoms: Some("drops at night too"),
            ..Default::default()
        };
        let case = update_case(&mut conn, CHAT, "case-1", 2, &only_symptoms)
            .await
            .unwrap();
        assert_eq!((case.current_rung, case.state), (2, CaseState::InProgress));
        assert_eq!(case.symptoms, "drops at night too");
    }

    #[tokio::test]
    async fn resolution_path_does_not_close_a_case_and_a_terminal_state_does() {
        let mut conn = mem_outputs_conn().await;
        open_case(&mut conn, &new_case("case-1", ROUTER))
            .await
            .unwrap();
        let up = CaseUpdate {
            state: Some(CaseState::AwaitingExternal),
            resolution_path: Some(ResolutionPath::WarrantyRma),
            current_rung: Some(4),
            ..Default::default()
        };
        let case = update_case(&mut conn, CHAT, "case-1", 1, &up)
            .await
            .unwrap();
        assert_eq!(case.resolution_path, Some(ResolutionPath::WarrantyRma));
        assert!(
            case.closed_at.is_none(),
            "warranty-rma stays open (decisions.id=508)"
        );
        assert_eq!(
            list_open_cases_for_entity(&mut conn, ROUTER, None)
                .await
                .unwrap()
                .len(),
            1
        );

        let close = CaseUpdate {
            state: Some(CaseState::Closed),
            outcome: Some("RMA done"),
            ..Default::default()
        };
        let closed = update_case(&mut conn, CHAT, "case-1", 2, &close)
            .await
            .unwrap();
        assert!(closed.closed_at.is_some());
        assert_eq!(
            closed.resolution_path,
            Some(ResolutionPath::WarrantyRma),
            "path is kept"
        );
        assert!(list_open_cases_for_entity(&mut conn, ROUTER, None)
            .await
            .unwrap()
            .is_empty());

        // A closed case takes no further writes of any kind.
        let again = CaseUpdate {
            current_rung: Some(1),
            ..Default::default()
        };
        let err = update_case(&mut conn, CHAT, "case-1", 3, &again)
            .await
            .unwrap_err();
        assert!(matches!(err, CaseStoreError::CaseClosed(_)), "{err:?}");
        let err = append_case_strategy(&mut conn, CHAT, &strategy("case-1", "r", "a", 1))
            .await
            .unwrap_err();
        assert!(matches!(err, CaseStoreError::CaseClosed(_)));
        let err = link_case_entity(&mut conn, CHAT, "case-1", MODEM)
            .await
            .unwrap_err();
        assert!(matches!(err, CaseStoreError::CaseClosed(_)));
        assert_eq!(
            count(&mut conn, "case_entities").await,
            1,
            "the refused link left nothing"
        );
    }

    #[tokio::test]
    async fn update_case_validates_input_and_missing_cases() {
        let mut conn = mem_outputs_conn().await;
        open_case(&mut conn, &new_case("case-1", ROUTER))
            .await
            .unwrap();
        let empty = CaseUpdate::default();
        assert!(matches!(
            update_case(&mut conn, CHAT, "case-1", 1, &empty)
                .await
                .unwrap_err(),
            CaseStoreError::Validation(_)
        ));
        for rung in [0, 6, -1] {
            let up = CaseUpdate {
                current_rung: Some(rung),
                ..Default::default()
            };
            assert!(matches!(
                update_case(&mut conn, CHAT, "case-1", 1, &up)
                    .await
                    .unwrap_err(),
                CaseStoreError::Validation(_)
            ));
        }
        let bad_json = CaseUpdate {
            durable_facts_json: Some("{not json"),
            ..Default::default()
        };
        assert!(matches!(
            update_case(&mut conn, CHAT, "case-1", 1, &bad_json)
                .await
                .unwrap_err(),
            CaseStoreError::Validation(_)
        ));
        let long = "x".repeat(CASE_TEXT_MAX_BYTES + 1);
        let too_long = CaseUpdate {
            symptoms: Some(&long),
            ..Default::default()
        };
        assert!(matches!(
            update_case(&mut conn, CHAT, "case-1", 1, &too_long)
                .await
                .unwrap_err(),
            CaseStoreError::Validation(_)
        ));
        let up = CaseUpdate {
            current_rung: Some(2),
            ..Default::default()
        };
        assert!(matches!(
            update_case(&mut conn, CHAT, "missing", 1, &up)
                .await
                .unwrap_err(),
            CaseStoreError::NotFound(_)
        ));
        let kept = get_case(&mut conn, "case-1").await.unwrap().unwrap();
        assert_eq!(kept.version, 1, "no refused update changed the case");
    }

    #[tokio::test]
    async fn open_cases_are_listed_per_entity_and_role_oldest_first() {
        let mut conn = mem_outputs_conn().await;
        open_case(&mut conn, &new_case("case-a", ROUTER))
            .await
            .unwrap();
        open_case(&mut conn, &new_case("case-b", MODEM))
            .await
            .unwrap();
        link_case_entity(&mut conn, CHAT, "case-b", ROUTER)
            .await
            .unwrap();

        let any = list_open_cases_for_entity(&mut conn, ROUTER, None)
            .await
            .unwrap();
        assert_eq!(
            any.iter().map(|c| c.case_id.as_str()).collect::<Vec<_>>(),
            ["case-a", "case-b"]
        );
        let primary = list_open_cases_for_entity(&mut conn, ROUTER, Some(EntityRole::Primary))
            .await
            .unwrap();
        assert_eq!(primary.len(), 1);
        assert_eq!(primary[0].case_id, "case-a");
        let affected = list_open_cases_for_entity(&mut conn, ROUTER, Some(EntityRole::Affected))
            .await
            .unwrap();
        assert_eq!(affected[0].case_id, "case-b");
        assert!(list_open_cases_for_entity(&mut conn, "nobody", None)
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn the_open_cases_index_is_partial_and_used() {
        let mut conn = mem_outputs_conn().await;
        let plan: Vec<(i64, i64, i64, String)> = sqlx::query_as(&format!(
            "EXPLAIN QUERY PLAN SELECT case_id FROM case_records
             WHERE lifecycle_state IN {OPEN_STATES_SQL} ORDER BY opened_at"
        ))
        .fetch_all(&mut conn)
        .await
        .unwrap();
        let text = plan
            .iter()
            .map(|r| r.3.as_str())
            .collect::<Vec<_>>()
            .join(" | ");
        assert!(text.contains("idx_case_records_open"), "plan: {text}");
        let def: String = sqlx::query_scalar(
            "SELECT sql FROM sqlite_master WHERE name = 'idx_case_records_open'",
        )
        .fetch_one(&mut conn)
        .await
        .unwrap();
        assert!(def.contains("WHERE"), "the index must be partial: {def}");
    }

    // -- entity links ---------------------------------------------------------

    #[tokio::test]
    async fn linking_an_entity_is_idempotent_and_the_primary_stays_unique() {
        let mut conn = mem_outputs_conn().await;
        open_case(&mut conn, &new_case("case-1", ROUTER))
            .await
            .unwrap();
        assert_eq!(
            link_case_entity(&mut conn, CHAT, "case-1", MODEM)
                .await
                .unwrap(),
            LinkOutcome::Linked
        );
        assert_eq!(
            link_case_entity(&mut conn, CHAT, "case-1", MODEM)
                .await
                .unwrap(),
            LinkOutcome::AlreadyLinked
        );
        // Linking the primary again as "affected" does not demote or duplicate it.
        assert_eq!(
            link_case_entity(&mut conn, CHAT, "case-1", ROUTER)
                .await
                .unwrap(),
            LinkOutcome::AlreadyLinked
        );
        let links = list_case_entities(&mut conn, "case-1").await.unwrap();
        assert_eq!(links.len(), 2);
        assert_eq!(
            links
                .iter()
                .filter(|l| l.role == EntityRole::Primary)
                .count(),
            1
        );

        let second_primary = sqlx::query(
            "INSERT INTO case_entities (case_id, entity_id, role, linked_at)
             VALUES ('case-1', 'someone-else', 'primary', 't')",
        )
        .execute(&mut conn)
        .await;
        assert!(second_primary.is_err(), "one primary per case");
        assert!(matches!(
            link_case_entity(&mut conn, CHAT, "missing", MODEM)
                .await
                .unwrap_err(),
            CaseStoreError::NotFound(_)
        ));
    }

    #[tokio::test]
    async fn child_rows_cascade_when_a_case_or_delta_is_deleted() {
        // items.id=888's entity purge deletes a case and relies on this. The
        // store itself has no delete function.
        let mut conn = mem_outputs_conn().await;
        open_case(&mut conn, &new_case("case-1", ROUTER))
            .await
            .unwrap();
        link_case_entity(&mut conn, CHAT, "case-1", MODEM)
            .await
            .unwrap();
        append_case_strategy(&mut conn, CHAT, &strategy("case-1", "r", "a", 1))
            .await
            .unwrap();
        record_config_delta(
            &mut conn,
            &NewConfigDelta {
                delta_id: "d1",
                context_key: CHAT,
                session_reference: "s",
                description: "dns",
                relevance_tags: &[],
                entity_ids: &[ROUTER],
                changed_at: None,
                recorded_by_run: "r",
            },
        )
        .await
        .unwrap();

        sqlx::query("DELETE FROM case_records WHERE case_id = 'case-1'")
            .execute(&mut conn)
            .await
            .unwrap();
        sqlx::query("DELETE FROM config_deltas WHERE delta_id = 'd1'")
            .execute(&mut conn)
            .await
            .unwrap();
        for t in ["case_entities", "case_strategies", "config_delta_entities"] {
            assert_eq!(count(&mut conn, t).await, 0, "{t}");
        }
    }

    // -- strategies -----------------------------------------------------------

    #[tokio::test]
    async fn strategies_append_in_order_and_dedupe_on_run_action_attempt() {
        let mut conn = mem_outputs_conn().await;
        open_case(&mut conn, &new_case("case-1", ROUTER))
            .await
            .unwrap();

        let first =
            append_case_strategy(&mut conn, CHAT, &strategy("case-1", "run-a", "diagnose", 1))
                .await
                .unwrap();
        let AppendOutcome::Appended { strategy_id: id1 } = first else {
            panic!("expected Appended")
        };

        // A retry of the same step attempt, even with different text, writes nothing.
        let mut retry = strategy("case-1", "run-a", "diagnose", 1);
        retry.strategy = "something else entirely";
        retry.user_signal = Some("I tried that already");
        let dup = append_case_strategy(&mut conn, CHAT, &retry).await.unwrap();
        assert_eq!(dup, AppendOutcome::Duplicate { strategy_id: id1 });

        // A different attempt, run or action is a new entry.
        for (run, action, attempt) in [
            ("run-a", "diagnose", 2),
            ("run-b", "diagnose", 1),
            ("run-a", "verify", 1),
        ] {
            let out =
                append_case_strategy(&mut conn, CHAT, &strategy("case-1", run, action, attempt))
                    .await
                    .unwrap();
            assert!(
                matches!(out, AppendOutcome::Appended { .. }),
                "{run}/{action}/{attempt}"
            );
        }

        let log = list_case_strategies(&mut conn, "case-1").await.unwrap();
        assert_eq!(log.len(), 4);
        assert!(
            log.windows(2).all(|w| w[0].strategy_id < w[1].strategy_id),
            "append order"
        );
        assert_eq!(
            log[0].strategy, "isolate the network layer",
            "the duplicate did not overwrite"
        );
        assert!(log[0].user_signal.is_none());
    }

    #[tokio::test]
    async fn a_duplicate_append_on_a_since_closed_case_is_still_a_duplicate() {
        let mut conn = mem_outputs_conn().await;
        open_case(&mut conn, &new_case("case-1", ROUTER))
            .await
            .unwrap();
        append_case_strategy(&mut conn, CHAT, &strategy("case-1", "run-a", "diagnose", 1))
            .await
            .unwrap();
        let close = CaseUpdate {
            state: Some(CaseState::Resolved),
            ..Default::default()
        };
        update_case(&mut conn, CHAT, "case-1", 1, &close)
            .await
            .unwrap();
        let retry =
            append_case_strategy(&mut conn, CHAT, &strategy("case-1", "run-a", "diagnose", 1))
                .await
                .unwrap();
        assert!(matches!(retry, AppendOutcome::Duplicate { .. }));
    }

    #[tokio::test]
    async fn strategy_input_is_validated() {
        let mut conn = mem_outputs_conn().await;
        open_case(&mut conn, &new_case("case-1", ROUTER))
            .await
            .unwrap();
        let mut s = strategy("case-1", "r", "a", -1);
        assert!(matches!(
            append_case_strategy(&mut conn, CHAT, &s).await.unwrap_err(),
            CaseStoreError::Validation(_)
        ));
        s = strategy("case-1", "r", "a", 1);
        s.strategy = "";
        assert!(matches!(
            append_case_strategy(&mut conn, CHAT, &s).await.unwrap_err(),
            CaseStoreError::Validation(_)
        ));
        let long = "x".repeat(STRATEGY_TEXT_MAX_BYTES + 1);
        s = strategy("case-1", "r", "a", 1);
        s.outcome = &long;
        assert!(matches!(
            append_case_strategy(&mut conn, CHAT, &s).await.unwrap_err(),
            CaseStoreError::Validation(_)
        ));
        assert!(matches!(
            append_case_strategy(&mut conn, CHAT, &strategy("missing", "r", "a", 1))
                .await
                .unwrap_err(),
            CaseStoreError::NotFound(_)
        ));
        assert_eq!(count(&mut conn, "case_strategies").await, 0);
    }

    // -- subject label --------------------------------------------------------

    #[tokio::test]
    async fn refresh_subject_label_updates_primary_subject_cases_only() {
        let mut conn = mem_outputs_conn().await;
        open_case(&mut conn, &new_case("case-a", ROUTER))
            .await
            .unwrap();
        open_case(&mut conn, &new_case("case-b", ROUTER))
            .await
            .unwrap();
        open_case(&mut conn, &new_case("case-c", MODEM))
            .await
            .unwrap();
        link_case_entity(&mut conn, CHAT, "case-c", ROUTER)
            .await
            .unwrap();

        let n = refresh_subject_label(&mut conn, ROUTER, "Attic router")
            .await
            .unwrap();
        assert_eq!(n, 2, "only the cases whose primary subject is the router");
        for (id, label) in [
            ("case-a", "Attic router"),
            ("case-b", "Attic router"),
            ("case-c", "Living room router"),
        ] {
            assert_eq!(
                get_case(&mut conn, id)
                    .await
                    .unwrap()
                    .unwrap()
                    .subject_label,
                label,
                "{id}"
            );
        }
        // Same label again changes nothing, and the version was never bumped.
        assert_eq!(
            refresh_subject_label(&mut conn, ROUTER, "Attic router")
                .await
                .unwrap(),
            0
        );
        assert_eq!(
            get_case(&mut conn, "case-a")
                .await
                .unwrap()
                .unwrap()
                .version,
            1
        );
        // It works on closed cases too: the label is a display value of the record.
        let close = CaseUpdate {
            state: Some(CaseState::Closed),
            ..Default::default()
        };
        update_case(&mut conn, CHAT, "case-a", 1, &close)
            .await
            .unwrap();
        assert_eq!(
            refresh_subject_label(&mut conn, ROUTER, "Garage router")
                .await
                .unwrap(),
            2
        );
        assert!(matches!(
            refresh_subject_label(&mut conn, ROUTER, &"x".repeat(SUBJECT_LABEL_MAX_BYTES + 1))
                .await
                .unwrap_err(),
            CaseStoreError::Validation(_)
        ));
    }

    // -- config deltas ---------------------------------------------------------

    fn delta<'a>(
        id: &'a str,
        entities: &'a [&'a str],
        changed_at: Option<&'a str>,
    ) -> NewConfigDelta<'a> {
        NewConfigDelta {
            delta_id: id,
            context_key: CHAT,
            session_reference: "session-1",
            description: "changed the DNS servers",
            relevance_tags: &["dns", "network"],
            entity_ids: entities,
            changed_at,
            recorded_by_run: "run-a",
        }
    }

    fn at(y: i32, m: u32, d: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, m, d, 12, 0, 0).unwrap()
    }

    #[tokio::test]
    async fn config_deltas_are_idempotent_and_start_believed_active() {
        let mut conn = mem_outputs_conn().await;
        let out = record_config_delta(&mut conn, &delta("d1", &[ROUTER, MODEM, ROUTER], None))
            .await
            .unwrap();
        assert_eq!(out, DeltaOutcome::Created);
        assert_eq!(
            count(&mut conn, "config_delta_entities").await,
            2,
            "a repeated entity is one link"
        );
        let again = record_config_delta(&mut conn, &delta("d1", &[ROUTER], None))
            .await
            .unwrap();
        assert_eq!(again, DeltaOutcome::Existing);
        assert_eq!(count(&mut conn, "config_deltas").await, 1);
        assert_eq!(
            count(&mut conn, "config_delta_entities").await,
            2,
            "the retry changed nothing"
        );

        let listed = list_config_deltas_for_entity(&mut conn, ROUTER, Utc::now())
            .await
            .unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].stored_status, DeltaStatus::BelievedActive);
        assert_eq!(listed[0].relevance_tags, ["dns", "network"]);
    }

    #[tokio::test]
    async fn a_config_delta_needs_an_entity_and_a_valid_timestamp() {
        let mut conn = mem_outputs_conn().await;
        assert!(matches!(
            record_config_delta(&mut conn, &delta("d1", &[], None))
                .await
                .unwrap_err(),
            CaseStoreError::Validation(_)
        ));
        assert!(matches!(
            record_config_delta(&mut conn, &delta("d1", &[ROUTER], Some("last tuesday")))
                .await
                .unwrap_err(),
            CaseStoreError::Validation(_)
        ));
        assert_eq!(count(&mut conn, "config_deltas").await, 0);
    }

    #[test]
    fn effective_status_degrades_at_read_time_after_ninety_days() {
        let changed = "2026-01-01T12:00:00+00:00";
        let day90 = at(2026, 1, 1) + Duration::days(CONFIG_DELTA_DEGRADE_DAYS);
        let in_window = |now| {
            effective_status(
                DeltaStatus::BelievedActive,
                changed,
                now,
                CONFIG_DELTA_DEGRADE_DAYS,
            )
        };
        assert_eq!(
            in_window(day90),
            DeltaStatus::BelievedActive,
            "exactly 90 days is in window"
        );
        assert_eq!(
            in_window(day90 + Duration::seconds(1)),
            DeltaStatus::Unknown
        );
        assert_eq!(in_window(at(2026, 1, 2)), DeltaStatus::BelievedActive);

        // Confirmed-active is on the same clock (see the doc comment).
        assert_eq!(
            effective_status(
                DeltaStatus::ConfirmedActive,
                changed,
                day90 + Duration::days(1),
                90
            ),
            DeltaStatus::Unknown
        );
        // believed-changed and unknown never move on read.
        for s in [DeltaStatus::BelievedChanged, DeltaStatus::Unknown] {
            assert_eq!(effective_status(s, changed, at(2030, 1, 1), 90), s);
        }
        // An unreadable clock reads as unknown, never as still active.
        assert_eq!(
            effective_status(DeltaStatus::BelievedActive, "garbage", at(2026, 1, 2), 90),
            DeltaStatus::Unknown
        );
        // A clock in the future (skew) does not degrade.
        assert_eq!(
            effective_status(
                DeltaStatus::BelievedActive,
                "2027-01-01T00:00:00+00:00",
                at(2026, 1, 2),
                90
            ),
            DeltaStatus::BelievedActive
        );
    }

    #[tokio::test]
    async fn degradation_is_computed_on_read_and_never_written_back() {
        let mut conn = mem_outputs_conn().await;
        record_config_delta(
            &mut conn,
            &delta("d1", &[ROUTER], Some("2026-01-01T12:00:00+00:00")),
        )
        .await
        .unwrap();

        let early = list_config_deltas_for_entity(&mut conn, ROUTER, at(2026, 2, 1))
            .await
            .unwrap();
        assert_eq!(early[0].effective_status, DeltaStatus::BelievedActive);
        let late = list_config_deltas_for_entity(&mut conn, ROUTER, at(2026, 6, 1))
            .await
            .unwrap();
        assert_eq!(late[0].effective_status, DeltaStatus::Unknown);
        assert_eq!(
            late[0].stored_status,
            DeltaStatus::BelievedActive,
            "the stored status is untouched"
        );
        let stored: String =
            sqlx::query_scalar("SELECT status FROM config_delta_entities WHERE delta_id = 'd1'")
                .fetch_one(&mut conn)
                .await
                .unwrap();
        assert_eq!(stored, "believed-active");
    }

    #[tokio::test]
    async fn confirming_a_delta_restarts_its_clock_and_events_mark_it_believed_changed() {
        let mut conn = mem_outputs_conn().await;
        record_config_delta(
            &mut conn,
            &delta("d1", &[ROUTER], Some("2026-01-01T12:00:00+00:00")),
        )
        .await
        .unwrap();
        let june = at(2026, 6, 1);
        assert_eq!(
            list_config_deltas_for_entity(&mut conn, ROUTER, june)
                .await
                .unwrap()[0]
                .effective_status,
            DeltaStatus::Unknown
        );

        set_delta_entity_status(
            &mut conn,
            CHAT,
            "d1",
            ROUTER,
            DeltaStatus::ConfirmedActive,
            Some("2026-05-20T12:00:00+00:00"),
        )
        .await
        .unwrap();
        let row = &list_config_deltas_for_entity(&mut conn, ROUTER, june)
            .await
            .unwrap()[0];
        assert_eq!(
            (row.stored_status, row.effective_status),
            (DeltaStatus::ConfirmedActive, DeltaStatus::ConfirmedActive)
        );

        set_delta_entity_status(
            &mut conn,
            CHAT,
            "d1",
            ROUTER,
            DeltaStatus::BelievedChanged,
            None,
        )
        .await
        .unwrap();
        let row = &list_config_deltas_for_entity(&mut conn, ROUTER, Utc::now())
            .await
            .unwrap()[0];
        assert_eq!(row.effective_status, DeltaStatus::BelievedChanged);

        assert!(matches!(
            set_delta_entity_status(&mut conn, CHAT, "d1", "nobody", DeltaStatus::Unknown, None)
                .await
                .unwrap_err(),
            CaseStoreError::NotFound(_)
        ));
        let bad =
            sqlx::query("UPDATE config_delta_entities SET status = 'maybe' WHERE delta_id = 'd1'")
                .execute(&mut conn)
                .await;
        assert!(bad.is_err(), "status names are closed by the schema");
    }

    #[test]
    fn delta_status_names_are_exactly_those_of_decision_510() {
        for n in [
            "believed-active",
            "confirmed-active",
            "believed-changed",
            "unknown",
        ] {
            assert_eq!(DeltaStatus::parse(n).unwrap().as_str(), n);
        }
        assert!(DeltaStatus::parse("active").is_none());
    }

    // -- the latch --------------------------------------------------------------

    /// Every write that runs inside a chat, as a closure result, for one chat.
    async fn attempt_every_gated_write(
        conn: &mut SqliteConnection,
        ctx: &str,
    ) -> Vec<Result<(), CaseStoreError>> {
        let mut out = Vec::new();
        let mut c = new_case("case-gated", ROUTER);
        c.context_key = ctx;
        out.push(open_case(conn, &c).await.map(|_| ()));
        let up = CaseUpdate {
            current_rung: Some(3),
            ..Default::default()
        };
        out.push(update_case(conn, ctx, "case-1", 1, &up).await.map(|_| ()));
        out.push(
            link_case_entity(conn, ctx, "case-1", "entity-new")
                .await
                .map(|_| ()),
        );
        out.push(
            append_case_strategy(conn, ctx, &strategy("case-1", "r", "gated", 9))
                .await
                .map(|_| ()),
        );
        let mut d = delta("d-gated", &[ROUTER], None);
        d.context_key = ctx;
        out.push(record_config_delta(conn, &d).await.map(|_| ()));
        out.push(
            set_delta_entity_status(conn, ctx, "d-seed", ROUTER, DeltaStatus::Unknown, None).await,
        );
        out
    }

    async fn seed_open_chat(conn: &mut SqliteConnection) {
        open_case(conn, &new_case("case-1", ROUTER)).await.unwrap();
        record_config_delta(conn, &delta("d-seed", &[ROUTER], None))
            .await
            .unwrap();
    }

    async fn totals(conn: &mut SqliteConnection) -> Vec<i64> {
        let mut v = Vec::new();
        for t in [
            "case_records",
            "case_entities",
            "case_strategies",
            "config_deltas",
            "config_delta_entities",
        ] {
            v.push(count(conn, t).await);
        }
        v
    }

    #[tokio::test]
    async fn once_the_chat_is_latched_every_case_write_is_refused_and_nothing_is_written() {
        let mut conn = mem_outputs_conn().await;
        seed_open_chat(&mut conn).await;
        latch_write_suppressed(&mut conn, CHAT, FOCUS, "run-a")
            .await
            .unwrap();
        let before = totals(&mut conn).await;
        let case_before = get_case(&mut conn, "case-1").await.unwrap().unwrap();

        let results = attempt_every_gated_write(&mut conn, CHAT).await;
        assert_eq!(results.len(), 6);
        for (i, r) in results.iter().enumerate() {
            assert!(
                matches!(r, Err(CaseStoreError::Latch(LatchError::Suppressed))),
                "gated write #{i}: {r:?}"
            );
        }
        assert_eq!(totals(&mut conn).await, before);
        assert_eq!(
            get_case(&mut conn, "case-1").await.unwrap().unwrap(),
            case_before
        );
        let stored: String = sqlx::query_scalar(
            "SELECT status FROM config_delta_entities WHERE delta_id = 'd-seed'",
        )
        .fetch_one(&mut conn)
        .await
        .unwrap();
        assert_eq!(stored, "believed-active");

        // Reads are not gated.
        assert!(get_case(&mut conn, "case-1").await.unwrap().is_some());
        assert_eq!(
            list_config_deltas_for_entity(&mut conn, ROUTER, Utc::now())
                .await
                .unwrap()
                .len(),
            1
        );
        // A chat that was never latched is unaffected.
        let mut other = new_case("case-other", ROUTER);
        other.context_key = "chat-2";
        assert!(matches!(
            open_case(&mut conn, &other).await.unwrap(),
            OpenOutcome::Created(_)
        ));
        // The label refresh has no chat and is not latched (see the module header).
        assert_eq!(
            refresh_subject_label(&mut conn, ROUTER, "Renamed")
                .await
                .unwrap(),
            2
        );
    }

    #[tokio::test]
    async fn an_unreadable_latch_refuses_every_case_write_and_leaves_no_row() {
        let mut conn = mem_outputs_conn().await;
        seed_open_chat(&mut conn).await;
        let before = totals(&mut conn).await;
        sqlx::query("ALTER TABLE session_variables RENAME TO session_variables_gone")
            .execute(&mut conn)
            .await
            .unwrap();

        let results = attempt_every_gated_write(&mut conn, CHAT).await;
        for (i, r) in results.iter().enumerate() {
            assert!(
                matches!(r, Err(CaseStoreError::Latch(LatchError::Unreadable(_)))),
                "gated write #{i} must fail closed: {r:?}"
            );
        }
        assert_eq!(totals(&mut conn).await, before);
        sqlx::query("BEGIN")
            .execute(&mut conn)
            .await
            .expect("no transaction left open");
        sqlx::query("ROLLBACK").execute(&mut conn).await.unwrap();
    }

    #[tokio::test]
    async fn the_session_store_and_the_case_store_share_one_latch() {
        let mut conn = mem_outputs_conn().await;
        seed_open_chat(&mut conn).await;
        // Latched through the session store, refused through both.
        latch_write_suppressed(&mut conn, CHAT, FOCUS, "run-a")
            .await
            .unwrap();
        let w = SessionWrite {
            context_key: CHAT,
            focus_id: FOCUS,
            name: "rung",
            value: "1",
            written_by_run: "run-a",
            expected_version: None,
        };
        assert!(matches!(
            put_session_variable(&mut conn, &w).await.unwrap_err(),
            SessionStoreError::Latch(LatchError::Suppressed)
        ));
        let up = CaseUpdate {
            current_rung: Some(2),
            ..Default::default()
        };
        assert!(matches!(
            update_case(&mut conn, CHAT, "case-1", 1, &up)
                .await
                .unwrap_err(),
            CaseStoreError::Latch(LatchError::Suppressed)
        ));
    }

    #[tokio::test]
    async fn context_and_id_inputs_are_validated_before_any_write() {
        let mut conn = mem_outputs_conn().await;
        let mut c = new_case("case-1", ROUTER);
        c.context_key = "";
        assert!(matches!(
            open_case(&mut conn, &c).await.unwrap_err(),
            CaseStoreError::Validation(_)
        ));
        c = new_case("", ROUTER);
        assert!(matches!(
            open_case(&mut conn, &c).await.unwrap_err(),
            CaseStoreError::Validation(_)
        ));
        c = new_case("case-1", "");
        assert!(matches!(
            open_case(&mut conn, &c).await.unwrap_err(),
            CaseStoreError::Validation(_)
        ));
        let long_label = "x".repeat(SUBJECT_LABEL_MAX_BYTES + 1);
        c = new_case("case-1", ROUTER);
        c.subject_label = &long_label;
        assert!(matches!(
            open_case(&mut conn, &c).await.unwrap_err(),
            CaseStoreError::Validation(_)
        ));
        assert_eq!(count(&mut conn, "case_records").await, 0);
    }
}
