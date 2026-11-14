// src-tauri/src/commands/device_library.rs
//
// items.id=891 stage 5 (decisions.id=509, 520, 940 ruling 2, 944): the user's own
// commands for the DEVICE LIBRARY of a focus_level Focus (Tech Support): the
// devices it keeps about the user's equipment, what it recorded about each, and
// the ways the user can correct or delete it. None of these is a model path and
// none is consent-gated: a user's own edit or deletion always works, like
// session_clear, whether or not consent was given, declined or withdrawn (what a
// withdrawal leaves is "visible and deletable", decisions.id=944 R4, and this is
// the surface that shows and deletes it).
//
// R1 SET (every device is named by its HANDLE, the E-XXXXXXXX the model also sees,
// never an internal id; a handle that is malformed, another Focus's, an archived or
// purge-marked device's, or invented is device_not_found):
//   list_devices(persona_id, focus_id)
//       the Focus's active devices (handle, name, kind, fact and open-case counts).
//       Finishes any half-done purge first (entity_purge::reconcile_persona).
//   get_device_sheet(persona_id, focus_id, device)
//       the READ-ONLY derived view of decisions.id=520: the catalogued facts in
//       catalogue order, the cases kept for the device (newest first) and its
//       configuration changes. It is not an output and has no storage of its own.
//   preview_device_purge(persona_id, focus_id, device)
//       exactly what a purge would remove, counted, for the confirmation.
//   purge_device(persona_id, focus_id, device)
//       the hard purge (persistence::entity_purge). Stops the Focus's live runs
//       first and refuses with nothing deleted if one ignores the stop.
//   purge_all_devices(persona_id, focus_id)
//       the SEPARATE offered control after a withdrawal (decisions.id=944 (8)):
//       every device the Focus owns, and the Focus's parked runs are cancelled.
//   rename_device(persona_id, focus_id, device, new_name)
//       renames the device (personal.db) and then refreshes the subject label of
//       every case that has it as its primary subject (case_store::
//       refresh_subject_label, outputs.db). A retry after a stopped rename
//       completes it: the second step is idempotent and runs even if the name
//       already changed.
//   set_device_fact(persona_id, focus_id, device, field, value)
//       the user's edit of one catalogued fact (conductor::device_fields).
//   clear_device_fact(persona_id, focus_id, device, field)
//       removes every version of one catalogued fact.
// NOT in R1 (decisions.id=891 plan): archiving a device, creating one by hand,
// per-case or per-change delete, editing the case and change parts of the sheet,
// and any visibility setter.
//
// EVERY command: takes writer_gate::command first (a key rotation waits for it),
// derives the user and the data key from the unlocked session through
// persona::session_user_and_key (no user_id argument exists to spoof), checks the
// Persona id is a UUID and belongs to that user (require_persona, private and
// written so items.id=638 can lift it), and does its work in a *_for_session body
// with no Tauri state, so each is tested through its body.
//
// Error strings are short codes for the frontend: persona_not_found,
// invalid_focus_id, device_not_found, invalid_device_name, name_in_use,
// unknown_field, invalid_value, runs_still_running, catalogue_unavailable.
// Anything else is a store error's own text.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use specta::Type;
use tauri::State;

use crate::auth::registry::KeyRegistry;
use crate::commands::persona::session_user_and_key;
use crate::conductor::device_fields::{self, ValueError, SUBJECT_CLASS_FIELD};
use crate::persistence::case_store::{self, EntityRole};
use crate::persistence::entity_purge::{self, PurgeCounts, PurgeError};
use crate::persistence::handles::{self, HandleError};
use crate::persistence::output_store::open_outputs_db;
use crate::persistence::owned_entity_store::{self as owned, OwnedEntityError};
use crate::persistence::persona_store;
use crate::persistence::personal_store::open_personal_db;

/// The most cases and configuration changes one device sheet lists.
const SHEET_CASES_MAX: i64 = 50;
const SHEET_CHANGES_MAX: usize = 50;

// ---------------------------------------------------------------------------
// IPC types
// ---------------------------------------------------------------------------

/// One device in the list. `handle` names it in every other command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct DeviceSummary {
    pub handle: String,
    pub display_name: String,
    /// The subject class fact (device:endpoint:laptop), when set.
    pub subject_class: Option<String>,
    /// Active catalogued facts.
    pub fact_count: i32,
    /// Open cases that have this device as their primary subject.
    pub open_case_count: i32,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct DeviceFact {
    pub field_name: String,
    pub label: String,
    pub category: String,
    pub value: String,
    pub recorded_at: String,
}

/// A case kept for the device, as a summary. No id: R1 has no per-case action.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct DeviceCase {
    pub state: String,
    pub subject_label: String,
    pub symptoms: String,
    pub current_rung: i32,
    pub outcome: Option<String>,
    pub opened_at: String,
    pub closed_at: Option<String>,
}

/// A configuration change recorded for the device. `status` is the effective one
/// (a stored believed-active older than the degradation window reads as unknown).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct DeviceChange {
    pub description: String,
    pub status: String,
    pub changed_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct DeviceSheet {
    pub device: DeviceSummary,
    pub facts: Vec<DeviceFact>,
    pub cases: Vec<DeviceCase>,
    pub changes: Vec<DeviceChange>,
}

/// What a purge removed, or (from the preview) will remove.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct DevicePurgeSummary {
    pub cases: i32,
    pub strategies: i32,
    pub changes: i32,
    pub facts: i32,
}

fn summary_of(c: &PurgeCounts) -> DevicePurgeSummary {
    let n = |v: i64| i32::try_from(v).unwrap_or(i32::MAX);
    DevicePurgeSummary {
        cases: n(c.cases),
        strategies: n(c.strategies),
        changes: n(c.deltas),
        facts: n(c.facts),
    }
}

// ---------------------------------------------------------------------------
// Validation and error helpers
// ---------------------------------------------------------------------------

/// The Persona id must be a canonical UUID AND belong to the signed-in user. The
/// two failures answer alike (persona_not_found). Private on purpose, so
/// items.id=638 can lift it into one shared check with the other copies.
async fn require_persona(
    pool: &sqlx::SqlitePool,
    user_id: &str,
    persona_id: &str,
) -> Result<(), String> {
    let canonical = uuid::Uuid::parse_str(persona_id)
        .map(|u| u.hyphenated().to_string() == persona_id)
        .unwrap_or(false);
    if !canonical {
        return Err("persona_not_found".to_owned());
    }
    match persona_store::is_user_in_persona(pool, user_id, persona_id).await {
        Ok(true) => Ok(()),
        Ok(false) => Err("persona_not_found".to_owned()),
        Err(e) => Err(e.to_string()),
    }
}

/// A Focus slug: lowercase letters, digits, '-' and '_', 1 to 64 characters.
fn require_focus_id(focus_id: &str) -> Result<(), String> {
    let ok = !focus_id.is_empty()
        && focus_id.len() <= 64
        && focus_id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_');
    if ok {
        Ok(())
    } else {
        Err("invalid_focus_id".to_owned())
    }
}

fn owned_err(e: OwnedEntityError) -> String {
    match e {
        OwnedEntityError::NotFound => "device_not_found".to_owned(),
        OwnedEntityError::NameInUse => "name_in_use".to_owned(),
        OwnedEntityError::Validation(_) => "invalid_device_name".to_owned(),
        OwnedEntityError::Value(ValueError::UnknownField) => "unknown_field".to_owned(),
        OwnedEntityError::Value(_) => "invalid_value".to_owned(),
        OwnedEntityError::Catalogue(_) => "catalogue_unavailable".to_owned(),
        other => other.to_string(),
    }
}

fn purge_err(e: PurgeError) -> String {
    match e {
        PurgeError::NotFound => "device_not_found".to_owned(),
        PurgeError::RunsStillRunning => "runs_still_running".to_owned(),
        PurgeError::Validation(m) if m == "unknown_field" => m,
        other => other.to_string(),
    }
}

fn handle_err(e: HandleError) -> String {
    match e {
        HandleError::Malformed(_) | HandleError::NotFound(_) | HandleError::Ambiguous(_) => {
            "device_not_found".to_owned()
        }
        HandleError::Owned(e) => owned_err(e),
        other => other.to_string(),
    }
}

/// The two databases of a Persona, opened for one command.
struct Dbs {
    personal: sqlx::SqliteConnection,
    outputs: sqlx::SqliteConnection,
}

async fn open_dbs(user_id: &str, persona_id: &str, key_hex: &str) -> Result<Dbs, String> {
    Ok(Dbs {
        personal: open_personal_db(user_id, persona_id, key_hex)
            .await
            .map_err(|e| e.to_string())?,
        outputs: open_outputs_db(user_id, persona_id, key_hex)
            .await
            .map_err(|e| e.to_string())?,
    })
}

/// The entity id behind a device handle: an ACTIVE device this Focus owns.
async fn resolve(db: &mut Dbs, focus_id: &str, device: &str) -> Result<String, String> {
    handles::resolve_entity_handle(&mut db.personal, focus_id, device)
        .await
        .map_err(handle_err)
}

async fn summarize(
    db: &mut Dbs,
    focus_id: &str,
    entity: &crate::persistence::entity_store::Entity,
) -> Result<DeviceSummary, String> {
    let facts = owned::list_active_facts(&mut db.personal, focus_id, &entity.id)
        .await
        .map_err(owned_err)?;
    let open = case_store::list_open_cases_for_entity(
        &mut db.outputs,
        &entity.id,
        Some(EntityRole::Primary),
        Some(focus_id),
    )
    .await
    .map_err(|e| e.to_string())?;
    Ok(DeviceSummary {
        handle: handles::entity_handle(focus_id, &entity.id),
        display_name: entity.display_name.clone(),
        subject_class: facts
            .iter()
            .find(|f| f.field_name == SUBJECT_CLASS_FIELD)
            .map(|f| f.value.clone()),
        fact_count: i32::try_from(facts.len()).unwrap_or(i32::MAX),
        open_case_count: i32::try_from(open.len()).unwrap_or(i32::MAX),
        created_at: entity.created_at.clone(),
    })
}

// ---------------------------------------------------------------------------
// Bodies (no Tauri state)
// ---------------------------------------------------------------------------

pub(crate) async fn list_devices_for_session(
    pool: &sqlx::SqlitePool,
    user_id: &str,
    key_hex: &str,
    persona_id: &str,
    focus_id: &str,
) -> Result<Vec<DeviceSummary>, String> {
    require_persona(pool, user_id, persona_id).await?;
    require_focus_id(focus_id)?;
    // Finish any half-done purge before the list is read, so a device the user
    // deleted never reappears and its leftover rows are gone. Best effort.
    if let Err(e) = entity_purge::reconcile_persona(user_id, persona_id, key_hex).await {
        log::warn!("device_library: reconcile before the list failed: {e}");
    }
    let mut db = open_dbs(user_id, persona_id, key_hex).await?;
    let devices = owned::list_owned_active(&mut db.personal, focus_id)
        .await
        .map_err(owned_err)?;
    let mut out = Vec::with_capacity(devices.len());
    for d in &devices {
        out.push(summarize(&mut db, focus_id, d).await?);
    }
    Ok(out)
}

pub(crate) async fn get_device_sheet_for_session(
    pool: &sqlx::SqlitePool,
    user_id: &str,
    key_hex: &str,
    persona_id: &str,
    focus_id: &str,
    device: &str,
) -> Result<DeviceSheet, String> {
    require_persona(pool, user_id, persona_id).await?;
    require_focus_id(focus_id)?;
    let mut db = open_dbs(user_id, persona_id, key_hex).await?;
    let entity_id = resolve(&mut db, focus_id, device).await?;
    let entity = owned::get_owned_active(&mut db.personal, focus_id, &entity_id)
        .await
        .map_err(owned_err)?;
    let catalogue = device_fields::registry().map_err(|_| "catalogue_unavailable".to_owned())?;

    let facts = owned::list_active_facts(&mut db.personal, focus_id, &entity_id)
        .await
        .map_err(owned_err)?
        .into_iter()
        .filter_map(|f| {
            let def = catalogue.field(&f.field_name)?;
            Some(DeviceFact {
                label: def.label.clone(),
                category: def.category.clone(),
                field_name: f.field_name,
                value: f.value,
                recorded_at: f.created_at,
            })
        })
        .collect();
    let cases = case_store::list_cases_for_entity(
        &mut db.outputs,
        &entity_id,
        Some(focus_id),
        SHEET_CASES_MAX,
    )
    .await
    .map_err(|e| e.to_string())?
    .into_iter()
    .map(|c| DeviceCase {
        state: c.state.as_str().to_owned(),
        subject_label: c.subject_label,
        symptoms: c.symptoms,
        current_rung: i32::try_from(c.current_rung).unwrap_or(i32::MAX),
        outcome: c.outcome,
        opened_at: c.opened_at,
        closed_at: c.closed_at,
    })
    .collect();
    let changes = case_store::list_config_deltas_for_entity(
        &mut db.outputs,
        &entity_id,
        Some(focus_id),
        chrono::Utc::now(),
    )
    .await
    .map_err(|e| e.to_string())?
    .into_iter()
    .take(SHEET_CHANGES_MAX)
    .map(|d| DeviceChange {
        description: d.description,
        status: d.effective_status.as_str().to_owned(),
        changed_at: d.changed_at,
    })
    .collect();

    Ok(DeviceSheet {
        device: summarize(&mut db, focus_id, &entity).await?,
        facts,
        cases,
        changes,
    })
}

pub(crate) async fn preview_device_purge_for_session(
    pool: &sqlx::SqlitePool,
    user_id: &str,
    key_hex: &str,
    persona_id: &str,
    focus_id: &str,
    device: &str,
) -> Result<DevicePurgeSummary, String> {
    require_persona(pool, user_id, persona_id).await?;
    require_focus_id(focus_id)?;
    let mut db = open_dbs(user_id, persona_id, key_hex).await?;
    let entity_id = resolve(&mut db, focus_id, device).await?;
    let counts =
        entity_purge::preview_purge(&mut db.personal, &mut db.outputs, focus_id, &entity_id)
            .await
            .map_err(purge_err)?;
    Ok(summary_of(&counts))
}

/// `wait` is how long the Focus's live runs get to end after the stop is asked
/// (the command passes entity_purge::RUN_STOP_WAIT; tests shorten it).
pub(crate) async fn purge_device_for_session(
    pool: &sqlx::SqlitePool,
    user_id: &str,
    key_hex: &str,
    persona_id: &str,
    focus_id: &str,
    device: &str,
    wait: Duration,
) -> Result<DevicePurgeSummary, String> {
    require_persona(pool, user_id, persona_id).await?;
    require_focus_id(focus_id)?;
    let entity_id = {
        let mut db = open_dbs(user_id, persona_id, key_hex).await?;
        resolve(&mut db, focus_id, device).await?
    };
    let counts =
        entity_purge::purge_device(user_id, persona_id, key_hex, focus_id, &entity_id, wait)
            .await
            .map_err(purge_err)?;
    Ok(summary_of(&counts))
}

pub(crate) async fn purge_all_devices_for_session(
    pool: &sqlx::SqlitePool,
    user_id: &str,
    key_hex: &str,
    persona_id: &str,
    focus_id: &str,
    wait: Duration,
) -> Result<DevicePurgeSummary, String> {
    require_persona(pool, user_id, persona_id).await?;
    require_focus_id(focus_id)?;
    let counts = entity_purge::purge_all_devices(user_id, persona_id, key_hex, focus_id, wait)
        .await
        .map_err(purge_err)?;
    Ok(summary_of(&counts))
}

pub(crate) async fn rename_device_for_session(
    pool: &sqlx::SqlitePool,
    user_id: &str,
    key_hex: &str,
    persona_id: &str,
    focus_id: &str,
    device: &str,
    new_name: &str,
) -> Result<DeviceSummary, String> {
    require_persona(pool, user_id, persona_id).await?;
    require_focus_id(focus_id)?;
    let mut db = open_dbs(user_id, persona_id, key_hex).await?;
    let entity_id = resolve(&mut db, focus_id, device).await?;
    // 1. The device (personal.db). Naming it what it is already named succeeds, so
    //    a retry after a stop reaches step 2.
    let entity = owned::rename_owned(&mut db.personal, focus_id, &entity_id, new_name)
        .await
        .map_err(owned_err)?;
    // 2. The labels of the cases it is the primary subject of (outputs.db).
    //    Idempotent, and not latch-gated: a rename has no chat.
    case_store::refresh_subject_label(&mut db.outputs, &entity_id, &entity.display_name)
        .await
        .map_err(|e| e.to_string())?;
    summarize(&mut db, focus_id, &entity).await
}

#[allow(clippy::too_many_arguments)] // the session, the Persona, the Focus, the device, a field and a value: no natural grouping, as in the other _for_session bodies
pub(crate) async fn set_device_fact_for_session(
    pool: &sqlx::SqlitePool,
    user_id: &str,
    key_hex: &str,
    persona_id: &str,
    focus_id: &str,
    device: &str,
    field: &str,
    value: &str,
) -> Result<DeviceFact, String> {
    require_persona(pool, user_id, persona_id).await?;
    require_focus_id(focus_id)?;
    let mut db = open_dbs(user_id, persona_id, key_hex).await?;
    let entity_id = resolve(&mut db, focus_id, device).await?;
    owned::put_owned_fact(
        &mut db.personal,
        focus_id,
        persona_id,
        &entity_id,
        field,
        value,
    )
    .await
    .map_err(owned_err)?;
    let catalogue = device_fields::registry().map_err(|_| "catalogue_unavailable".to_owned())?;
    let def = catalogue
        .field(field.trim())
        .ok_or_else(|| "unknown_field".to_owned())?;
    let fact = owned::list_active_facts(&mut db.personal, focus_id, &entity_id)
        .await
        .map_err(owned_err)?
        .into_iter()
        .find(|f| f.field_name == def.name)
        .ok_or_else(|| "device_not_found".to_owned())?;
    Ok(DeviceFact {
        label: def.label.clone(),
        category: def.category.clone(),
        field_name: fact.field_name,
        value: fact.value,
        recorded_at: fact.created_at,
    })
}

pub(crate) async fn clear_device_fact_for_session(
    pool: &sqlx::SqlitePool,
    user_id: &str,
    key_hex: &str,
    persona_id: &str,
    focus_id: &str,
    device: &str,
    field: &str,
) -> Result<(), String> {
    require_persona(pool, user_id, persona_id).await?;
    require_focus_id(focus_id)?;
    let mut db = open_dbs(user_id, persona_id, key_hex).await?;
    let entity_id = resolve(&mut db, focus_id, device).await?;
    entity_purge::clear_owned_fact(&mut db.personal, focus_id, &entity_id, field)
        .await
        .map_err(purge_err)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

#[tauri::command]
#[specta::specta]
pub async fn list_devices(
    persona_id: String,
    focus_id: String,
    key_registry: State<'_, KeyRegistry>,
    pool: State<'_, sqlx::SqlitePool>,
) -> Result<Vec<DeviceSummary>, String> {
    let _lease = crate::auth::writer_gate::command(&key_registry).await?;
    let (user_id, key_hex) = session_user_and_key(&key_registry).await?;
    list_devices_for_session(&pool, &user_id, &key_hex, &persona_id, &focus_id).await
}

#[tauri::command]
#[specta::specta]
pub async fn get_device_sheet(
    persona_id: String,
    focus_id: String,
    device: String,
    key_registry: State<'_, KeyRegistry>,
    pool: State<'_, sqlx::SqlitePool>,
) -> Result<DeviceSheet, String> {
    let _lease = crate::auth::writer_gate::command(&key_registry).await?;
    let (user_id, key_hex) = session_user_and_key(&key_registry).await?;
    get_device_sheet_for_session(&pool, &user_id, &key_hex, &persona_id, &focus_id, &device).await
}

#[tauri::command]
#[specta::specta]
pub async fn preview_device_purge(
    persona_id: String,
    focus_id: String,
    device: String,
    key_registry: State<'_, KeyRegistry>,
    pool: State<'_, sqlx::SqlitePool>,
) -> Result<DevicePurgeSummary, String> {
    let _lease = crate::auth::writer_gate::command(&key_registry).await?;
    let (user_id, key_hex) = session_user_and_key(&key_registry).await?;
    preview_device_purge_for_session(&pool, &user_id, &key_hex, &persona_id, &focus_id, &device)
        .await
}

/// Deletes the device and what QuietRabbit recorded about it. Stops the Focus's
/// live runs first and waits for them (up to a minute); Err("runs_still_running")
/// means one ignored the stop and NOTHING was deleted: try again.
#[tauri::command]
#[specta::specta]
pub async fn purge_device(
    persona_id: String,
    focus_id: String,
    device: String,
    key_registry: State<'_, KeyRegistry>,
    pool: State<'_, sqlx::SqlitePool>,
) -> Result<DevicePurgeSummary, String> {
    let _lease = crate::auth::writer_gate::command(&key_registry).await?;
    let (user_id, key_hex) = session_user_and_key(&key_registry).await?;
    purge_device_for_session(
        &pool,
        &user_id,
        &key_hex,
        &persona_id,
        &focus_id,
        &device,
        entity_purge::RUN_STOP_WAIT,
    )
    .await
}

/// The offered control after a withdrawal: every device the Focus owns, and the
/// Focus's parked runs cancelled. Err("runs_still_running") means nothing was
/// deleted.
#[tauri::command]
#[specta::specta]
pub async fn purge_all_devices(
    persona_id: String,
    focus_id: String,
    key_registry: State<'_, KeyRegistry>,
    pool: State<'_, sqlx::SqlitePool>,
) -> Result<DevicePurgeSummary, String> {
    let _lease = crate::auth::writer_gate::command(&key_registry).await?;
    let (user_id, key_hex) = session_user_and_key(&key_registry).await?;
    purge_all_devices_for_session(
        &pool,
        &user_id,
        &key_hex,
        &persona_id,
        &focus_id,
        entity_purge::RUN_STOP_WAIT,
    )
    .await
}

#[tauri::command]
#[specta::specta]
pub async fn rename_device(
    persona_id: String,
    focus_id: String,
    device: String,
    new_name: String,
    key_registry: State<'_, KeyRegistry>,
    pool: State<'_, sqlx::SqlitePool>,
) -> Result<DeviceSummary, String> {
    let _lease = crate::auth::writer_gate::command(&key_registry).await?;
    let (user_id, key_hex) = session_user_and_key(&key_registry).await?;
    rename_device_for_session(
        &pool,
        &user_id,
        &key_hex,
        &persona_id,
        &focus_id,
        &device,
        &new_name,
    )
    .await
}

#[tauri::command]
#[specta::specta]
pub async fn set_device_fact(
    persona_id: String,
    focus_id: String,
    device: String,
    field: String,
    value: String,
    key_registry: State<'_, KeyRegistry>,
    pool: State<'_, sqlx::SqlitePool>,
) -> Result<DeviceFact, String> {
    let _lease = crate::auth::writer_gate::command(&key_registry).await?;
    let (user_id, key_hex) = session_user_and_key(&key_registry).await?;
    set_device_fact_for_session(
        &pool,
        &user_id,
        &key_hex,
        &persona_id,
        &focus_id,
        &device,
        &field,
        &value,
    )
    .await
}

#[tauri::command]
#[specta::specta]
pub async fn clear_device_fact(
    persona_id: String,
    focus_id: String,
    device: String,
    field: String,
    key_registry: State<'_, KeyRegistry>,
    pool: State<'_, sqlx::SqlitePool>,
) -> Result<(), String> {
    let _lease = crate::auth::writer_gate::command(&key_registry).await?;
    let (user_id, key_hex) = session_user_and_key(&key_registry).await?;
    clear_device_fact_for_session(
        &pool,
        &user_id,
        &key_hex,
        &persona_id,
        &focus_id,
        &device,
        &field,
    )
    .await
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------
//
// UUID fixtures throughout (require_persona refuses anything else). Real shared.db,
// real encrypted personal.db and outputs.db files under a scratch QR_DATA_ROOT.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::registry::key_hex;
    use crate::persistence::case_store::{NewCase, NewConfigDelta};
    use crate::persistence::session_store::{
        latch_write_suppressed, put_session_variable, SessionWrite,
    };
    use crate::test_support::{mock_app_with_registry, populate_registry, ENV_MUTEX};
    use tauri::Manager;

    const FOCUS: &str = "tech-support";
    const OTHER: &str = "other-focus";
    const CHAT: &str = "chat-1";
    const MASTER_KEY: [u8; crate::auth::kdf::MASTER_KEY_LEN] =
        [0xCDu8; crate::auth::kdf::MASTER_KEY_LEN];
    const SHORT: Duration = Duration::from_millis(150);

    struct Env {
        _tempdir: tempfile::TempDir,
        _lock: tokio::sync::MutexGuard<'static, ()>,
        saved_root: Option<String>,
        pool: sqlx::SqlitePool,
        user: String,
        persona: String,
        key: String,
    }

    impl Drop for Env {
        fn drop(&mut self) {
            match &self.saved_root {
                Some(v) => std::env::set_var("QR_DATA_ROOT", v),
                None => std::env::remove_var("QR_DATA_ROOT"),
            }
        }
    }

    async fn add_user(pool: &sqlx::SqlitePool, name: &str) -> String {
        let id = uuid::Uuid::new_v4().to_string();
        crate::auth::user_store::create_user(
            pool,
            &id,
            name,
            "user",
            false,
            &[0u8; crate::auth::kdf::SALT_LEN],
            crate::auth::kdf::DEFAULT_ARGON2_MEMORY_KIB,
            crate::auth::kdf::DEFAULT_ARGON2_ITERATIONS,
            crate::auth::kdf::DEFAULT_ARGON2_PARALLELISM,
            &[0u8; 32],
        )
        .await
        .expect("create_user");
        id
    }

    async fn add_persona(pool: &sqlx::SqlitePool, user: &str, key: &str) -> String {
        let id = uuid::Uuid::new_v4().to_string();
        persona_store::create_persona(pool, &id, "P", "personal", user, key, None)
            .await
            .expect("create_persona");
        id
    }

    async fn env() -> Env {
        let lock = ENV_MUTEX.lock().await;
        let saved_root = std::env::var("QR_DATA_ROOT").ok();
        let tempdir = tempfile::tempdir().expect("tempdir");
        std::env::set_var("QR_DATA_ROOT", tempdir.path());
        crate::persistence::migrations::migrate_shared_db()
            .await
            .expect("shared.db");
        let pool =
            sqlx::SqlitePool::connect_with(crate::providers::utils::connect_options_unencrypted(
                &crate::providers::utils::db_path_shared(),
            ))
            .await
            .expect("pool");
        let key = key_hex(&MASTER_KEY);
        let user = add_user(&pool, "Library User").await;
        let persona = add_persona(&pool, &user, &key).await;
        Env {
            _tempdir: tempdir,
            _lock: lock,
            saved_root,
            pool,
            user,
            persona,
            key,
        }
    }

    async fn dbs(e: &Env) -> Dbs {
        open_dbs(&e.user, &e.persona, &e.key).await.unwrap()
    }

    /// A device of `focus`, returning (entity id, handle).
    async fn device(e: &Env, focus: &str, name: &str, key: &str) -> (String, String) {
        let mut db = dbs(e).await;
        let id = owned::create_subject(&mut db.personal, focus, &e.persona, name, None, key)
            .await
            .unwrap();
        let handle = handles::entity_handle(focus, &id);
        (id, handle)
    }

    async fn put(e: &Env, id: &str, field: &str, value: &str) {
        let mut db = dbs(e).await;
        owned::put_owned_fact(&mut db.personal, FOCUS, &e.persona, id, field, value)
            .await
            .unwrap();
    }

    async fn open_case(e: &Env, case_id: &str, focus: &str, entity: &str, label: &str) {
        let mut db = dbs(e).await;
        case_store::open_case(
            &mut db.outputs,
            &NewCase {
                case_id,
                focus_id: focus,
                context_key: CHAT,
                primary_entity_id: entity,
                subject_label: label,
                symptoms: "drops at night",
                opened_by_run: "run-1",
            },
        )
        .await
        .unwrap();
    }

    async fn list(e: &Env) -> Result<Vec<DeviceSummary>, String> {
        list_devices_for_session(&e.pool, &e.user, &e.key, &e.persona, FOCUS).await
    }

    async fn sheet(e: &Env, handle: &str) -> Result<DeviceSheet, String> {
        get_device_sheet_for_session(&e.pool, &e.user, &e.key, &e.persona, FOCUS, handle).await
    }

    async fn fact_rows(db: &mut Dbs, entity: &str, field: &str) -> i64 {
        sqlx::query_scalar(
            "SELECT COUNT(*) FROM entity_facts WHERE entity_id = ? AND field_name = ?",
        )
        .bind(entity)
        .bind(field)
        .fetch_one(&mut db.personal)
        .await
        .unwrap()
    }

    // -- list and sheet ------------------------------------------------------

    #[tokio::test]
    async fn the_list_has_this_focus_active_devices_with_handles_and_counts() {
        let e = env().await;
        let (router, router_h) = device(&e, FOCUS, "Router", "r:1").await;
        let (_other, _) = device(&e, OTHER, "Camera", "r:2").await;
        put(&e, &router, "subject_class", "device:network:router").await;
        put(&e, &router, "make", "Netgear").await;
        open_case(&e, "case1", FOCUS, &router, "Router").await;

        let devices = list(&e).await.unwrap();
        assert_eq!(devices.len(), 1, "another Focus's device is not listed");
        let d = &devices[0];
        assert_eq!(d.handle, router_h);
        assert!(d.handle.starts_with("E-"));
        assert_eq!(d.display_name, "Router");
        assert_eq!(d.subject_class.as_deref(), Some("device:network:router"));
        assert_eq!((d.fact_count, d.open_case_count), (2, 1));
        // The response never carries an internal id.
        let json = serde_json::to_string(&devices).unwrap();
        assert!(!json.contains(&router));
    }

    #[tokio::test]
    async fn listing_finishes_a_half_done_purge_first() {
        let e = env().await;
        let (router, _) = device(&e, FOCUS, "Router", "r:1").await;
        let (_modem, modem_h) = device(&e, FOCUS, "Modem", "r:2").await;
        open_case(&e, "case1", FOCUS, &router, "Router").await;
        {
            // The purge stopped right after the outputs step.
            let mut db = dbs(&e).await;
            let mut hook = |s: entity_purge::PurgeStep| {
                if s == entity_purge::PurgeStep::AfterOutputs {
                    Err("x".to_owned())
                } else {
                    Ok(())
                }
            };
            entity_purge::purge_entity_with(
                &mut db.personal,
                &mut db.outputs,
                FOCUS,
                &router,
                &mut hook,
            )
            .await
            .unwrap_err();
        }
        let devices = list(&e).await.unwrap();
        assert_eq!(
            devices.iter().map(|d| d.handle.clone()).collect::<Vec<_>>(),
            [modem_h]
        );
        let mut db = dbs(&e).await;
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM entities WHERE id = ?")
            .bind(&router)
            .fetch_one(&mut db.personal)
            .await
            .unwrap();
        assert_eq!(n, 0, "the reconcile finished it");
    }

    #[tokio::test]
    async fn the_sheet_reads_facts_in_catalogue_order_with_cases_and_changes() {
        let e = env().await;
        let (router, h) = device(&e, FOCUS, "Router", "r:1").await;
        let (theirs, theirs_h) = device(&e, OTHER, "Camera", "r:2").await;
        put(&e, &router, "known_issues", "drops at night").await;
        put(&e, &router, "make", "Netgear").await;
        put(&e, &router, "serial_number", "SN-1").await;
        open_case(&e, "case1", FOCUS, &router, "Router").await;
        open_case(&e, "case2", FOCUS, &router, "Router again").await;
        open_case(&e, "case3", OTHER, &router, "another Focus's").await;
        {
            let mut db = dbs(&e).await;
            case_store::record_config_delta(
                &mut db.outputs,
                &NewConfigDelta {
                    delta_id: "d1",
                    focus_id: FOCUS,
                    source_case_id: Some("case1"),
                    context_key: CHAT,
                    session_reference: CHAT,
                    description: "changed the DNS servers",
                    relevance_tags: &[],
                    entity_ids: &[&router],
                    changed_at: None,
                    recorded_by_run: "run-1",
                },
            )
            .await
            .unwrap();
            case_store::record_config_delta(
                &mut db.outputs,
                &NewConfigDelta {
                    delta_id: "d-other",
                    focus_id: OTHER,
                    source_case_id: None,
                    context_key: CHAT,
                    session_reference: CHAT,
                    description: "another Focus's change",
                    relevance_tags: &[],
                    entity_ids: &[&router],
                    changed_at: None,
                    recorded_by_run: "run-1",
                },
            )
            .await
            .unwrap();
        }

        let s = sheet(&e, &h).await.unwrap();
        assert_eq!(s.device.handle, h);
        let fields: Vec<&str> = s.facts.iter().map(|f| f.field_name.as_str()).collect();
        assert_eq!(fields, ["make", "serial_number", "known_issues"]);
        assert_eq!(s.facts[0].label, "Make");
        assert_eq!(s.facts[0].category, "identity");
        assert_eq!(s.facts[1].value, "SN-1");
        // This Focus's two cases only, and only its change.
        assert_eq!(s.cases.len(), 2);
        assert!(s.cases.iter().all(|c| c.state == "open"));
        assert!(s.cases.iter().any(|c| c.subject_label == "Router again"));
        assert_eq!(s.changes.len(), 1);
        assert_eq!(s.changes[0].description, "changed the DNS servers");
        assert_eq!(s.changes[0].status, "believed-active");

        // A handle of another Focus's device, an invented one and junk are all
        // device_not_found.
        for bad in [theirs_h.as_str(), "E-00000000", "none", ""] {
            assert_eq!(
                sheet(&e, bad).await.unwrap_err(),
                "device_not_found",
                "{bad}"
            );
        }
        let _ = theirs;
    }

    // -- rename --------------------------------------------------------------

    #[tokio::test]
    async fn a_rename_changes_the_device_and_the_labels_of_the_cases_it_is_primary_of() {
        let e = env().await;
        let (router, h) = device(&e, FOCUS, "Router", "r:1").await;
        let (modem, _) = device(&e, FOCUS, "Modem", "r:2").await;
        open_case(&e, "primary", FOCUS, &router, "Router").await;
        open_case(&e, "affected", FOCUS, &modem, "Modem").await;
        {
            let mut db = dbs(&e).await;
            case_store::link_case_entity(&mut db.outputs, CHAT, "affected", &router)
                .await
                .unwrap();
        }

        let d = rename_device_for_session(
            &e.pool,
            &e.user,
            &e.key,
            &e.persona,
            FOCUS,
            &h,
            "  Living   room router ",
        )
        .await
        .unwrap();
        assert_eq!(d.display_name, "Living room router");
        assert_eq!(d.handle, h, "the handle follows the entity, not the name");

        let mut db = dbs(&e).await;
        let labels: Vec<(String, String)> =
            sqlx::query_as("SELECT case_id, subject_label FROM case_records ORDER BY case_id")
                .fetch_all(&mut db.outputs)
                .await
                .unwrap();
        assert_eq!(
            labels,
            [
                ("affected".to_owned(), "Modem".to_owned()),
                ("primary".to_owned(), "Living room router".to_owned()),
            ],
            "only a case that has the device as its PRIMARY subject is relabelled"
        );
    }

    #[tokio::test]
    async fn a_retried_rename_completes_a_stopped_one() {
        let e = env().await;
        let (router, h) = device(&e, FOCUS, "Router", "r:1").await;
        open_case(&e, "c1", FOCUS, &router, "Router").await;
        // The stop: step 1 done, step 2 not.
        {
            let mut db = dbs(&e).await;
            owned::rename_owned(&mut db.personal, FOCUS, &router, "New name")
                .await
                .unwrap();
        }
        let d =
            rename_device_for_session(&e.pool, &e.user, &e.key, &e.persona, FOCUS, &h, "New name")
                .await
                .unwrap();
        assert_eq!(d.display_name, "New name");
        let mut db = dbs(&e).await;
        let label: String =
            sqlx::query_scalar("SELECT subject_label FROM case_records WHERE case_id = 'c1'")
                .fetch_one(&mut db.outputs)
                .await
                .unwrap();
        assert_eq!(label, "New name");
    }

    #[tokio::test]
    async fn a_rename_refuses_a_taken_or_bad_name_and_changes_nothing() {
        let e = env().await;
        let (_router, h) = device(&e, FOCUS, "Router", "r:1").await;
        device(&e, FOCUS, "Modem", "r:2").await;
        device(&e, OTHER, "Camera", "r:3").await;
        for (name, code) in [
            ("modem", "name_in_use"),
            ("  MODEM ", "name_in_use"),
            ("", "invalid_device_name"),
            ("   ", "invalid_device_name"),
            ("a\0b", "invalid_device_name"),
        ] {
            let err =
                rename_device_for_session(&e.pool, &e.user, &e.key, &e.persona, FOCUS, &h, name)
                    .await
                    .unwrap_err();
            assert_eq!(err, code, "{name:?}");
        }
        // Another Focus's device may share a name: the names are per Focus.
        assert!(rename_device_for_session(
            &e.pool, &e.user, &e.key, &e.persona, FOCUS, &h, "Camera"
        )
        .await
        .is_ok());
        assert_eq!(
            rename_device_for_session(
                &e.pool,
                &e.user,
                &e.key,
                &e.persona,
                FOCUS,
                "E-00000000",
                "X"
            )
            .await
            .unwrap_err(),
            "device_not_found"
        );
    }

    // -- facts ---------------------------------------------------------------

    #[tokio::test]
    async fn the_user_sets_and_clears_catalogued_facts_without_consent_or_a_latch() {
        let e = env().await;
        let (router, h) = device(&e, FOCUS, "Router", "r:1").await;
        // A one-time chat is latched: nothing a Focus does saves, but the user's own
        // edit is not a Focus write.
        {
            let mut db = dbs(&e).await;
            latch_write_suppressed(&mut db.outputs, CHAT, FOCUS, "run-1")
                .await
                .unwrap();
        }
        let f = set_device_fact_for_session(
            &e.pool,
            &e.user,
            &e.key,
            &e.persona,
            FOCUS,
            &h,
            "firmware_version",
            " 1.0 ",
        )
        .await
        .unwrap();
        assert_eq!(
            (f.field_name.as_str(), f.value.as_str(), f.label.as_str()),
            ("firmware_version", "1.0", "Firmware version")
        );
        set_device_fact_for_session(
            &e.pool,
            &e.user,
            &e.key,
            &e.persona,
            FOCUS,
            &h,
            "firmware_version",
            "2.0",
        )
        .await
        .unwrap();
        assert_eq!(sheet(&e, &h).await.unwrap().facts[0].value, "2.0");

        // Clear removes every version, and clearing again is fine.
        let mut db = dbs(&e).await;
        assert_eq!(fact_rows(&mut db, &router, "firmware_version").await, 2);
        clear_device_fact_for_session(
            &e.pool,
            &e.user,
            &e.key,
            &e.persona,
            FOCUS,
            &h,
            "firmware_version",
        )
        .await
        .unwrap();
        assert_eq!(
            fact_rows(&mut db, &router, "firmware_version").await,
            0,
            "the superseded version went too"
        );
        clear_device_fact_for_session(
            &e.pool,
            &e.user,
            &e.key,
            &e.persona,
            FOCUS,
            &h,
            "firmware_version",
        )
        .await
        .unwrap();
        assert!(sheet(&e, &h).await.unwrap().facts.is_empty());
    }

    #[tokio::test]
    async fn a_fact_edit_refuses_an_unknown_field_a_bad_value_and_a_wrong_device() {
        let e = env().await;
        let (_r, h) = device(&e, FOCUS, "Router", "r:1").await;
        let (_c, theirs_h) = device(&e, OTHER, "Camera", "r:2").await;
        for (handle, field, value, code) in [
            (&h, "wifi_password", "hunter2", "unknown_field"),
            (&h, "notes", "free text", "unknown_field"),
            (&h, "make", "   ", "invalid_value"),
            (&h, "make", &"x".repeat(81), "invalid_value"),
            (&h, "subject_class", "Not A Class", "invalid_value"),
            (&theirs_h, "make", "Netgear", "device_not_found"),
            (
                &"E-00000000".to_owned(),
                "make",
                "Netgear",
                "device_not_found",
            ),
        ] {
            let err = set_device_fact_for_session(
                &e.pool, &e.user, &e.key, &e.persona, FOCUS, handle, field, value,
            )
            .await
            .unwrap_err();
            assert_eq!(err, code, "{field}={value:?}");
        }
        for (handle, field, code) in [
            (&h, "wifi_password", "unknown_field"),
            (&theirs_h, "make", "device_not_found"),
        ] {
            let err = clear_device_fact_for_session(
                &e.pool, &e.user, &e.key, &e.persona, FOCUS, handle, field,
            )
            .await
            .unwrap_err();
            assert_eq!(err, code, "{field}");
        }
        assert!(sheet(&e, &h).await.unwrap().facts.is_empty());
    }

    // -- purge ---------------------------------------------------------------

    #[tokio::test]
    async fn the_preview_matches_the_purge_and_the_purge_removes_the_device() {
        let e = env().await;
        let (router, h) = device(&e, FOCUS, "Router", "r:1").await;
        let (_modem, modem_h) = device(&e, FOCUS, "Modem", "r:2").await;
        put(&e, &router, "make", "Netgear").await;
        put(&e, &router, "model", "R7000").await;
        open_case(&e, "case1", FOCUS, &router, "Router").await;
        {
            let mut db = dbs(&e).await;
            put_session_variable(
                &mut db.outputs,
                &SessionWrite {
                    context_key: CHAT,
                    focus_id: FOCUS,
                    name: "active_case_id",
                    value: "case1",
                    written_by_run: "run-1",
                    expected_version: None,
                },
            )
            .await
            .unwrap();
        }

        let preview =
            preview_device_purge_for_session(&e.pool, &e.user, &e.key, &e.persona, FOCUS, &h)
                .await
                .unwrap();
        assert_eq!(
            preview,
            DevicePurgeSummary {
                cases: 1,
                strategies: 0,
                changes: 0,
                facts: 2
            }
        );
        assert_eq!(
            list(&e).await.unwrap().len(),
            2,
            "a preview deletes nothing"
        );

        let done = purge_device_for_session(&e.pool, &e.user, &e.key, &e.persona, FOCUS, &h, SHORT)
            .await
            .unwrap();
        assert_eq!(done, preview);
        let left: Vec<String> = list(&e)
            .await
            .unwrap()
            .into_iter()
            .map(|d| d.handle)
            .collect();
        assert_eq!(left, [modem_h]);
        // The device is gone for every command.
        for code in [
            sheet(&e, &h).await.unwrap_err(),
            preview_device_purge_for_session(&e.pool, &e.user, &e.key, &e.persona, FOCUS, &h)
                .await
                .unwrap_err(),
            purge_device_for_session(&e.pool, &e.user, &e.key, &e.persona, FOCUS, &h, SHORT)
                .await
                .unwrap_err(),
        ] {
            assert_eq!(code, "device_not_found");
        }
    }

    #[tokio::test]
    async fn a_run_that_ignores_the_stop_makes_purge_device_refuse_with_the_code() {
        let e = env().await;
        let (_r, h) = device(&e, FOCUS, "Router", "r:1").await;
        let lease =
            crate::auth::writer_gate::acquire(&e.user, crate::auth::writer_gate::Kind::FocusRun)
                .unwrap();
        lease.bind_focus(&e.persona, FOCUS);
        let err = purge_device_for_session(&e.pool, &e.user, &e.key, &e.persona, FOCUS, &h, SHORT)
            .await
            .unwrap_err();
        assert_eq!(err, "runs_still_running");
        let err = purge_all_devices_for_session(&e.pool, &e.user, &e.key, &e.persona, FOCUS, SHORT)
            .await
            .unwrap_err();
        assert_eq!(err, "runs_still_running");
        assert_eq!(list(&e).await.unwrap().len(), 1, "nothing was deleted");
        drop(lease);
        assert!(
            purge_device_for_session(&e.pool, &e.user, &e.key, &e.persona, FOCUS, &h, SHORT)
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn purge_all_devices_removes_every_device_of_the_focus_only() {
        let e = env().await;
        device(&e, FOCUS, "Router", "r:1").await;
        device(&e, FOCUS, "Modem", "r:2").await;
        let (_c, theirs_h) = device(&e, OTHER, "Camera", "r:3").await;
        let done =
            purge_all_devices_for_session(&e.pool, &e.user, &e.key, &e.persona, FOCUS, SHORT)
                .await
                .unwrap();
        assert_eq!(done.facts, 0);
        assert!(list(&e).await.unwrap().is_empty());
        let theirs =
            get_device_sheet_for_session(&e.pool, &e.user, &e.key, &e.persona, OTHER, &theirs_h)
                .await
                .unwrap();
        assert_eq!(
            theirs.device.display_name, "Camera",
            "another Focus's device stays"
        );
    }

    // -- who may call --------------------------------------------------------

    /// Another user's Persona, a non-UUID id and a bad Focus id are refused by every
    /// body with the same codes, and nothing is touched.
    #[tokio::test]
    async fn every_body_refuses_another_users_persona_a_bad_persona_id_and_a_bad_focus_id() {
        let e = env().await;
        let (_r, h) = device(&e, FOCUS, "Router", "r:1").await;
        let other_user = add_user(&e.pool, "Someone Else").await;
        let their_persona = add_persona(&e.pool, &other_user, &e.key).await;

        for persona in [
            their_persona.as_str(),
            "not-a-uuid",
            "",
            &e.persona.to_uppercase(),
        ] {
            let codes = vec![
                list_devices_for_session(&e.pool, &e.user, &e.key, persona, FOCUS)
                    .await
                    .map(|_| ()),
                get_device_sheet_for_session(&e.pool, &e.user, &e.key, persona, FOCUS, &h)
                    .await
                    .map(|_| ()),
                preview_device_purge_for_session(&e.pool, &e.user, &e.key, persona, FOCUS, &h)
                    .await
                    .map(|_| ()),
                purge_device_for_session(&e.pool, &e.user, &e.key, persona, FOCUS, &h, SHORT)
                    .await
                    .map(|_| ()),
                purge_all_devices_for_session(&e.pool, &e.user, &e.key, persona, FOCUS, SHORT)
                    .await
                    .map(|_| ()),
                rename_device_for_session(&e.pool, &e.user, &e.key, persona, FOCUS, &h, "X")
                    .await
                    .map(|_| ()),
                set_device_fact_for_session(
                    &e.pool, &e.user, &e.key, persona, FOCUS, &h, "make", "x",
                )
                .await
                .map(|_| ()),
                clear_device_fact_for_session(&e.pool, &e.user, &e.key, persona, FOCUS, &h, "make")
                    .await,
            ];
            for r in codes {
                assert_eq!(r.unwrap_err(), "persona_not_found", "{persona}");
            }
        }
        for focus in ["", "Tech Support", "tech/support", &"a".repeat(65)] {
            assert_eq!(
                list_devices_for_session(&e.pool, &e.user, &e.key, &e.persona, focus)
                    .await
                    .unwrap_err(),
                "invalid_focus_id"
            );
            assert_eq!(
                purge_all_devices_for_session(&e.pool, &e.user, &e.key, &e.persona, focus, SHORT)
                    .await
                    .unwrap_err(),
                "invalid_focus_id"
            );
        }
        assert_eq!(list(&e).await.unwrap().len(), 1, "nothing changed");
    }

    /// The commands themselves: not logged in is refused, logged in they derive the
    /// user and key from the session and act on that user's Persona.
    #[tokio::test]
    async fn the_commands_need_a_session_and_use_the_sessions_user() {
        let e = env().await;
        let (_r, h) = device(&e, FOCUS, "Router", "r:1").await;
        let app = mock_app_with_registry(e.pool.clone());
        let registry = app.state::<KeyRegistry>();
        let pool = app.state::<sqlx::SqlitePool>();

        let r = list_devices(
            e.persona.clone(),
            FOCUS.to_owned(),
            registry.clone(),
            pool.clone(),
        )
        .await;
        assert!(r.is_err(), "not logged in");
        let r = rename_device(
            e.persona.clone(),
            FOCUS.to_owned(),
            h.clone(),
            "X".to_owned(),
            registry.clone(),
            pool.clone(),
        )
        .await;
        assert!(r.is_err(), "not logged in");
        assert_eq!(list(&e).await.unwrap()[0].display_name, "Router");

        populate_registry(&registry, &e.user, MASTER_KEY).await;
        let listed = list_devices(
            e.persona.clone(),
            FOCUS.to_owned(),
            registry.clone(),
            pool.clone(),
        )
        .await
        .unwrap();
        assert_eq!(listed.len(), 1);
        let renamed = rename_device(
            e.persona.clone(),
            FOCUS.to_owned(),
            h.clone(),
            "Living room router".to_owned(),
            registry.clone(),
            pool.clone(),
        )
        .await
        .unwrap();
        assert_eq!(renamed.display_name, "Living room router");
        let s = get_device_sheet(
            e.persona.clone(),
            FOCUS.to_owned(),
            h.clone(),
            registry.clone(),
            pool.clone(),
        )
        .await
        .unwrap();
        assert_eq!(s.device.display_name, "Living room router");
        set_device_fact(
            e.persona.clone(),
            FOCUS.to_owned(),
            h.clone(),
            "make".to_owned(),
            "Netgear".to_owned(),
            registry.clone(),
            pool.clone(),
        )
        .await
        .unwrap();
        clear_device_fact(
            e.persona.clone(),
            FOCUS.to_owned(),
            h.clone(),
            "make".to_owned(),
            registry.clone(),
            pool.clone(),
        )
        .await
        .unwrap();
        let p = preview_device_purge(
            e.persona.clone(),
            FOCUS.to_owned(),
            h.clone(),
            registry.clone(),
            pool.clone(),
        )
        .await
        .unwrap();
        assert_eq!(p.cases, 0);
        purge_device(
            e.persona.clone(),
            FOCUS.to_owned(),
            h,
            registry.clone(),
            pool.clone(),
        )
        .await
        .unwrap();
        purge_all_devices(e.persona.clone(), FOCUS.to_owned(), registry, pool)
            .await
            .unwrap();
        assert!(list(&e).await.unwrap().is_empty());
    }

    // -- shape ---------------------------------------------------------------

    fn production() -> String {
        let src = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/commands/device_library.rs"),
        )
        .unwrap();
        src.split("\n#[cfg(test)]").next().unwrap().to_owned()
    }

    /// Each command takes the writer gate first, derives the user from the session,
    /// takes no user_id, key or AppHandle, and carries its own body.
    #[test]
    fn every_command_takes_the_gate_first_and_derives_the_user_from_the_session() {
        let src = production();
        let commands: Vec<&str> = src.split("#[tauri::command]").skip(1).collect();
        assert_eq!(commands.len(), 8, "the R1 set, no more");
        for body in commands {
            let sig_end = body.find("{\n").unwrap();
            let sig = &body[..sig_end];
            assert!(!sig.contains("user_id"), "no user_id argument: {sig}");
            assert!(!sig.contains("AppHandle"), "{sig}");
            assert!(!sig.contains("key_hex"), "no key argument: {sig}");
            let after = &body[sig_end..];
            let gate = after
                .find("writer_gate::command(&key_registry)")
                .expect("gate");
            let session = after
                .find("session_user_and_key(&key_registry)")
                .expect("session");
            assert!(gate < session, "the gate comes first");
            assert!(after.contains("_for_session("), "a _for_session body");
        }
    }

    /// The R1 set has no archive, no manual create, no visibility setter, and none of
    /// the commands consults or sets the collection consent or the latch.
    #[test]
    fn the_r1_set_has_no_archive_create_or_visibility_and_touches_no_consent() {
        let src = production();
        for forbidden in [
            "archive_device",
            "create_device",
            "set_device_visibility",
            "redact_identification",
            "hide_from_shared_surfaces",
            "requires_collection_consent",
            "get_collection_consent",
            "set_collection_consent",
            "consent_audit",
            "write_mode",
        ] {
            let hits: Vec<&str> = src
                .lines()
                .filter(|l| !l.trim_start().starts_with("//"))
                .filter(|l| l.contains(forbidden))
                .collect();
            assert!(hits.is_empty(), "`{forbidden}`: {hits:?}");
        }
        // The commands write no SQL of their own: they call the stores.
        let hits: Vec<&str> = src
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .filter(|l| l.contains("sqlx::query"))
            .collect();
        assert!(hits.is_empty(), "{hits:?}");
    }
}
