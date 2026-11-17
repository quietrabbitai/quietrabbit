// src-tauri/src/persistence/entity_purge.rs
//
// items.id=891 stage 4 (decisions.id=509, 939 section 7, 940 ruling 2): the HARD
// PURGE of a Focus-owned entity, the one module that deletes one. A device and
// what QuietRabbit recorded about it (its facts, the cases and configuration
// changes kept for it) can be removed for good, and a purge that stops halfway
// finishes. It ships before Tech Support can be enabled (decisions.id=940), and it
// is the whole entity purge: items.id=888 keeps clear_chat, the retention sweep,
// the resume pin and the rest of the session lifecycle, and does not build this.
//
// WHERE THE DATA LIVES. The entity and its facts are in the persona's personal.db.
// The cases, configuration changes and session variables that name it are in the
// persona's outputs.db, linked by a plain text entity id with no foreign key
// across the two files. Two databases cannot share a transaction, so the purge is
// ordered so that a stop between any two steps is safe and recoverable.
//
// THE ORDER (purge_entity_with)
//   1. MARK. personal.db: entities.status = user_deleted, with purge_started_at in
//      extra_metadata. This is the COMMIT POINT: from here the entity is invisible
//      to every reader that filters on active (the entity writers, the handle
//      resolver, the device list, the sync and share loaders), so nothing new can
//      attach to it, and the user's intent to delete is durable.
//   2. OUTPUTS. outputs.db, ONE `BEGIN IMMEDIATE` transaction:
//        session_variables  active_case_id rows whose value is a deleted case
//        case_records       every case the entity is the PRIMARY subject of, and
//                           every case left with no link at all (cascades
//                           case_entities and case_strategies; a delta's
//                           source_case_id becomes NULL by its foreign key)
//        case_entities      the entity's remaining links (an affected link on a
//                           case that stays)
//        config_delta_entities  the entity's rows
//        config_deltas      every delta left with no entity link
//   3. PERSONAL. personal.db, ONE `BEGIN IMMEDIATE` transaction:
//        entity_facts       every row of the entity, active and superseded
//        entity_relationships  rows at either end (a stub in R1: none)
//        dedup_candidates   rows naming it
//        entities           the row itself
//   Each step is idempotent for the entity id. A failure inside a transaction rolls
//   that transaction back. A failure BETWEEN steps leaves the entity marked, which
//   is exactly what reconcile looks for.
//
// RECONCILE (reconcile) finds the two shapes a stop can leave and finishes them,
// with no journal:
//   * MARKED: an owned entity with status user_deleted. Steps 2 and 3 run again.
//   * ORPHAN: an entity id in case_entities or config_delta_entities that has no
//     row in entities at all (a writer that named the entity just before the mark
//     committed, or a crash between steps 2 and 3 that already removed the row).
//     Step 2 runs for it.
//   It runs at login for every persona of the signed-in user (best effort, in a
//   supervised task), before the device list is read, and at the start of
//   purge_all_devices.
//
// WHAT A RUN IN FLIGHT DOES. A run of the Focus could name the entity again after
// step 2 (a published id it already holds). purge_device and purge_all_devices
// therefore STOP the Focus's live runs first (auth::writer_gate::
// request_stop_for_focus) and wait a bounded time for them to end. A run that
// ignores the stop leaves the wait unfinished and the purge REFUSES with nothing
// deleted and nothing marked (RunsStillRunning): the user can try again. Whatever
// slips past that, an entity writer refuses a non-active entity, the case and delta
// writers verify the entity (conductor::entity_writer), and reconcile sweeps the
// leftover links.
//
// purge_all_devices (a withdrawal that deletes) also CANCELS the Focus's parked
// runs (awaiting_user, awaiting_schedule and the other non-terminal states): a
// parked run keeps its step outputs in a snapshot and the chat window in
// focus_runs.user_input (and its user-turns-only copy in focus_runs.external_input,
// items.id=883), all of which can name the device, and cancel clears them all
// (output_store::cancel_focus_run). A single-device purge does not cancel them: a
// parked run of another device is not the user's to lose.
//
// WHAT A PURGE DOES NOT REACH, and the user is told so (the consent promise is
// worded "what QuietRabbit recorded about it", never "everything linked"):
// chat messages, run step outputs and outputs text that mention the device by
// name, the permanent disclosure_log, and preference rows keyed by a fact hash
// (pf_standing_preferences, consent_decisions). An owned entity is never copied to
// another Persona (personal_011.sql, the share and sync loaders), so there is
// nothing to delete there in R1.
//
// ONLY THIS MODULE DELETES an owned entity, its facts or the case and delta rows
// that name it. case_store.rs and session_store.rs keep their no-delete scans
// (session_case_tests.rs) and a scan here pins that no other production file
// deletes these tables.
//
// QUERY STYLE: runtime sqlx::query() only, never the query!() macros.

use std::time::Duration;

use sqlx::SqliteConnection;
use thiserror::Error;

use crate::persistence::entity_store::{get_entity_conn, list_entities_conn};
use crate::persistence::entity_store::{EntityFilter, OwnerFilter};
use crate::persistence::output_store::{self, open_outputs_db};
use crate::persistence::personal_store::{open_personal_db, PersonalStoreError};
use crate::persistence::session_store::{begin_write, finish_write};

/// How long a purge waits for the Focus's live runs to end after asking them to
/// stop (the same cooperative-stop grace a key rotation uses).
pub const RUN_STOP_WAIT: Duration = crate::auth::writer_gate::STOP_GRACE;

#[derive(Debug, Error)]
pub enum PurgeError {
    /// No such owned entity for this Focus (missing, unowned or another Focus's).
    #[error("no such device for this Focus")]
    NotFound,
    /// The Focus still has a run that ignored the stop. Nothing was changed.
    #[error("a run of this Focus is still going: nothing was deleted")]
    RunsStillRunning,
    /// A step run between or inside the writes returned an error (the failure
    /// injection of the tests, and nothing in production).
    #[error("the purge was interrupted: {0}")]
    Interrupted(String),
    #[error("Database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("{0}")]
    Store(#[from] PersonalStoreError),
    #[error("{0}")]
    Output(#[from] output_store::OutputStoreError),
    #[error("{0}")]
    Validation(String),
}

/// What a purge removed (or, from `preview_purge`, will remove).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PurgeCounts {
    /// Cases deleted (the entity was their primary subject, or they were left
    /// with no link).
    pub cases: i64,
    /// Strategy rows that went with those cases.
    pub strategies: i64,
    /// Links from a surviving case to the entity.
    pub case_links: i64,
    /// Configuration changes deleted (left with no entity link).
    pub deltas: i64,
    /// config_delta_entities rows of the entity.
    pub delta_links: i64,
    /// active_case_id session variables that pointed at a deleted case.
    pub session_refs: i64,
    /// entity_facts rows, active and superseded.
    pub facts: i64,
    pub relationships: i64,
    pub dedup_candidates: i64,
}

impl PurgeCounts {
    fn add(&mut self, o: &PurgeCounts) {
        self.cases += o.cases;
        self.strategies += o.strategies;
        self.case_links += o.case_links;
        self.deltas += o.deltas;
        self.delta_links += o.delta_links;
        self.session_refs += o.session_refs;
        self.facts += o.facts;
        self.relationships += o.relationships;
        self.dedup_candidates += o.dedup_candidates;
    }
}

/// The points at which a purge can be interrupted, for the failure-injection
/// tests. A hook is called AT each point and an Err stops the purge there. In
/// production the hook is a no-op.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PurgeStep {
    /// After the mark committed, before anything else.
    AfterMark,
    /// Inside the outputs.db transaction, after its deletes and before its COMMIT
    /// (an Err rolls the transaction back).
    InOutputsTransaction,
    /// After the outputs.db transaction committed, before the personal.db one.
    AfterOutputs,
    /// Inside the personal.db transaction, after its deletes and before its COMMIT.
    InPersonalTransaction,
}

pub type Hook<'a> = &'a mut (dyn FnMut(PurgeStep) -> Result<(), String> + Send);

fn interrupted(msg: String) -> PurgeError {
    PurgeError::Interrupted(msg)
}

fn check_focus(focus: &str) -> Result<(), PurgeError> {
    if focus.is_empty() || focus.len() > 64 {
        return Err(PurgeError::Validation(
            "focus_id must be a Focus slug".into(),
        ));
    }
    Ok(())
}

fn check_entity_id(id: &str) -> Result<(), PurgeError> {
    if id.is_empty() || id.len() > 128 {
        return Err(PurgeError::Validation("entity id is not valid".into()));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// outputs.db: the plan and the step
// ---------------------------------------------------------------------------

/// What the outputs.db step will remove for one entity id.
#[derive(Debug, Default)]
struct OutputsPlan {
    doomed_cases: Vec<String>,
    case_links: i64,
    strategies: i64,
    delta_ids: Vec<String>,
    doomed_deltas: Vec<String>,
    session_refs: i64,
}

async fn plan_outputs(
    conn: &mut SqliteConnection,
    entity_id: &str,
) -> Result<OutputsPlan, PurgeError> {
    // A case goes if the entity is its PRIMARY subject, or if removing the entity's
    // link would leave it with no link at all.
    let doomed_cases: Vec<String> = sqlx::query_scalar(
        "SELECT DISTINCT ce.case_id FROM case_entities ce
         WHERE ce.entity_id = ?1
           AND (ce.role = 'primary'
                OR NOT EXISTS (SELECT 1 FROM case_entities o
                                WHERE o.case_id = ce.case_id AND o.entity_id <> ?1))
         ORDER BY ce.case_id",
    )
    .bind(entity_id)
    .fetch_all(&mut *conn)
    .await?;
    let case_links: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM case_entities WHERE entity_id = ?")
            .bind(entity_id)
            .fetch_one(&mut *conn)
            .await?;

    let mut strategies = 0;
    let mut session_refs = 0;
    for case_id in &doomed_cases {
        strategies +=
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM case_strategies WHERE case_id = ?")
                .bind(case_id)
                .fetch_one(&mut *conn)
                .await?;
        session_refs += sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM session_variables WHERE name = 'active_case_id' AND value = ?",
        )
        .bind(case_id)
        .fetch_one(&mut *conn)
        .await?;
    }

    let delta_ids: Vec<String> = sqlx::query_scalar(
        "SELECT DISTINCT delta_id FROM config_delta_entities WHERE entity_id = ? ORDER BY delta_id",
    )
    .bind(entity_id)
    .fetch_all(&mut *conn)
    .await?;
    let mut doomed_deltas = Vec::new();
    for delta_id in &delta_ids {
        let other: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM config_delta_entities WHERE delta_id = ? AND entity_id <> ?",
        )
        .bind(delta_id)
        .bind(entity_id)
        .fetch_one(&mut *conn)
        .await?;
        if other == 0 {
            doomed_deltas.push(delta_id.clone());
        }
    }

    Ok(OutputsPlan {
        doomed_cases,
        case_links,
        strategies,
        delta_ids,
        doomed_deltas,
        session_refs,
    })
}

fn outputs_counts(plan: &OutputsPlan) -> PurgeCounts {
    PurgeCounts {
        cases: plan.doomed_cases.len() as i64,
        strategies: plan.strategies,
        case_links: plan.case_links,
        deltas: plan.doomed_deltas.len() as i64,
        delta_links: plan.delta_ids.len() as i64,
        session_refs: plan.session_refs,
        ..Default::default()
    }
}

/// Step 2: delete what outputs.db holds about `entity_id`, in one transaction.
/// Idempotent: for an id nothing names it deletes nothing. Not latch-gated and not
/// consent-gated: removing data is always the user's to do.
pub(crate) async fn purge_outputs_side(
    conn: &mut SqliteConnection,
    entity_id: &str,
    hook: Hook<'_>,
) -> Result<PurgeCounts, PurgeError> {
    check_entity_id(entity_id)?;
    begin_write(conn).await?;
    let result = async {
        let plan = plan_outputs(conn, entity_id).await?;
        for case_id in &plan.doomed_cases {
            sqlx::query(
                "DELETE FROM session_variables WHERE name = 'active_case_id' AND value = ?",
            )
            .bind(case_id)
            .execute(&mut *conn)
            .await?;
            // The cascade removes the case's case_entities and case_strategies.
            sqlx::query("DELETE FROM case_records WHERE case_id = ?")
                .bind(case_id)
                .execute(&mut *conn)
                .await?;
        }
        sqlx::query("DELETE FROM case_entities WHERE entity_id = ?")
            .bind(entity_id)
            .execute(&mut *conn)
            .await?;
        sqlx::query("DELETE FROM config_delta_entities WHERE entity_id = ?")
            .bind(entity_id)
            .execute(&mut *conn)
            .await?;
        for delta_id in &plan.doomed_deltas {
            sqlx::query(
                "DELETE FROM config_deltas
                 WHERE delta_id = ?
                   AND NOT EXISTS (SELECT 1 FROM config_delta_entities e
                                    WHERE e.delta_id = config_deltas.delta_id)",
            )
            .bind(delta_id)
            .execute(&mut *conn)
            .await?;
        }
        hook(PurgeStep::InOutputsTransaction).map_err(interrupted)?;
        Ok::<PurgeCounts, PurgeError>(outputs_counts(&plan))
    }
    .await;
    finish_write(conn, result).await
}

// ---------------------------------------------------------------------------
// personal.db: the mark and the step
// ---------------------------------------------------------------------------

/// Step 1: mark an owned entity for purge. Idempotent (a second mark rewrites the
/// stamp), and refuses an entity this Focus does not own.
pub(crate) async fn mark_for_purge(
    personal: &mut SqliteConnection,
    focus: &str,
    entity_id: &str,
) -> Result<(), PurgeError> {
    check_focus(focus)?;
    check_entity_id(entity_id)?;
    let done = sqlx::query(
        "UPDATE entities
         SET status = 'user_deleted',
             extra_metadata = json_set(COALESCE(extra_metadata, '{}'), '$.purge_started_at', ?)
         WHERE id = ? AND owner_focus = ?",
    )
    .bind(crate::providers::utils::now())
    .bind(entity_id)
    .bind(focus)
    .execute(&mut *personal)
    .await?;
    if done.rows_affected() == 0 {
        return Err(PurgeError::NotFound);
    }
    Ok(())
}

/// Step 3: delete the entity and what personal.db holds about it, in one
/// transaction. Only an owned entity that is marked for purge is deleted: an
/// unowned or active entity is left alone. Idempotent: an entity that is already
/// gone deletes nothing.
pub(crate) async fn purge_personal_side(
    personal: &mut SqliteConnection,
    entity_id: &str,
    hook: Hook<'_>,
) -> Result<PurgeCounts, PurgeError> {
    check_entity_id(entity_id)?;
    begin_write(personal).await?;
    let result = async {
        let marked: Option<i64> = sqlx::query_scalar(
            "SELECT 1 FROM entities
             WHERE id = ? AND owner_focus IS NOT NULL AND status = 'user_deleted'",
        )
        .bind(entity_id)
        .fetch_optional(&mut *personal)
        .await?;
        if marked.is_none() {
            return Ok::<PurgeCounts, PurgeError>(PurgeCounts::default());
        }
        let facts = sqlx::query("DELETE FROM entity_facts WHERE entity_id = ?")
            .bind(entity_id)
            .execute(&mut *personal)
            .await?
            .rows_affected() as i64;
        let relationships = sqlx::query(
            "DELETE FROM entity_relationships WHERE from_entity_id = ?1 OR to_entity_id = ?1",
        )
        .bind(entity_id)
        .execute(&mut *personal)
        .await?
        .rows_affected() as i64;
        let dedup =
            sqlx::query("DELETE FROM dedup_candidates WHERE record_id_a = ?1 OR record_id_b = ?1")
                .bind(entity_id)
                .execute(&mut *personal)
                .await?
                .rows_affected() as i64;
        sqlx::query(
            "DELETE FROM entities
             WHERE id = ? AND owner_focus IS NOT NULL AND status = 'user_deleted'",
        )
        .bind(entity_id)
        .execute(&mut *personal)
        .await?;
        hook(PurgeStep::InPersonalTransaction).map_err(interrupted)?;
        Ok(PurgeCounts {
            facts,
            relationships,
            dedup_candidates: dedup,
            ..Default::default()
        })
    }
    .await;
    finish_write(personal, result).await
}

// ---------------------------------------------------------------------------
// The purge, the preview, reconcile
// ---------------------------------------------------------------------------

/// The Focus's owned entity by id in ANY status (an archived or purge-marked one
/// can still be purged), or NotFound.
async fn owned_any_status(
    personal: &mut SqliteConnection,
    focus: &str,
    entity_id: &str,
) -> Result<(), PurgeError> {
    check_focus(focus)?;
    check_entity_id(entity_id)?;
    get_entity_conn(personal, entity_id)
        .await?
        .filter(|e| e.owner_focus.as_deref() == Some(focus))
        .map(|_| ())
        .ok_or(PurgeError::NotFound)
}

/// What purging this entity will remove, counted without removing anything (the
/// confirmation of the device library shows it before the user agrees).
pub async fn preview_purge(
    personal: &mut SqliteConnection,
    outputs: &mut SqliteConnection,
    focus: &str,
    entity_id: &str,
) -> Result<PurgeCounts, PurgeError> {
    owned_any_status(personal, focus, entity_id).await?;
    let plan = plan_outputs(outputs, entity_id).await?;
    let mut counts = outputs_counts(&plan);
    counts.facts = sqlx::query_scalar("SELECT COUNT(*) FROM entity_facts WHERE entity_id = ?")
        .bind(entity_id)
        .fetch_one(&mut *personal)
        .await?;
    counts.relationships = sqlx::query_scalar(
        "SELECT COUNT(*) FROM entity_relationships WHERE from_entity_id = ?1 OR to_entity_id = ?1",
    )
    .bind(entity_id)
    .fetch_one(&mut *personal)
    .await?;
    counts.dedup_candidates = sqlx::query_scalar(
        "SELECT COUNT(*) FROM dedup_candidates WHERE record_id_a = ?1 OR record_id_b = ?1",
    )
    .bind(entity_id)
    .fetch_one(&mut *personal)
    .await?;
    Ok(counts)
}

/// Purge one owned entity, with a hook at each interruption point. See the module
/// header for the order. The connections are the persona's personal.db and
/// outputs.db; the caller has already stopped the Focus's live runs
/// (purge_device does).
pub async fn purge_entity_with(
    personal: &mut SqliteConnection,
    outputs: &mut SqliteConnection,
    focus: &str,
    entity_id: &str,
    hook: Hook<'_>,
) -> Result<PurgeCounts, PurgeError> {
    owned_any_status(personal, focus, entity_id).await?;
    // 1. MARK: the commit point.
    mark_for_purge(personal, focus, entity_id).await?;
    hook(PurgeStep::AfterMark).map_err(interrupted)?;
    // 2. OUTPUTS.
    let mut counts = purge_outputs_side(outputs, entity_id, &mut *hook).await?;
    hook(PurgeStep::AfterOutputs).map_err(interrupted)?;
    // 3. PERSONAL.
    counts.add(&purge_personal_side(personal, entity_id, &mut *hook).await?);
    Ok(counts)
}

/// purge_entity_with and no interruption.
pub async fn purge_entity(
    personal: &mut SqliteConnection,
    outputs: &mut SqliteConnection,
    focus: &str,
    entity_id: &str,
) -> Result<PurgeCounts, PurgeError> {
    purge_entity_with(personal, outputs, focus, entity_id, &mut |_| Ok(())).await
}

/// What a reconcile found and finished.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReconcileReport {
    /// Marked entities whose purge was finished.
    pub resumed: usize,
    /// Entity ids named in outputs.db that no longer exist, whose rows were removed.
    pub orphans: usize,
    pub counts: PurgeCounts,
}

/// Finish what a stopped purge left (module header: MARKED and ORPHAN). Idempotent,
/// and touches nothing when there is nothing to finish. It never deletes a live
/// entity: a marked one is, by definition, being purged, and an orphan is an id
/// with no entity row at all.
pub async fn reconcile(
    personal: &mut SqliteConnection,
    outputs: &mut SqliteConnection,
) -> Result<ReconcileReport, PurgeError> {
    let mut report = ReconcileReport::default();

    let marked: Vec<String> = sqlx::query_scalar(
        "SELECT id FROM entities
         WHERE owner_focus IS NOT NULL AND status = 'user_deleted' ORDER BY id",
    )
    .fetch_all(&mut *personal)
    .await?;
    for id in &marked {
        report
            .counts
            .add(&purge_outputs_side(outputs, id, &mut |_| Ok(())).await?);
        report
            .counts
            .add(&purge_personal_side(personal, id, &mut |_| Ok(())).await?);
        report.resumed += 1;
    }

    let named: Vec<String> = sqlx::query_scalar(
        "SELECT entity_id FROM case_entities
         UNION SELECT entity_id FROM config_delta_entities ORDER BY 1",
    )
    .fetch_all(&mut *outputs)
    .await?;
    for id in named {
        let exists: Option<i64> = sqlx::query_scalar("SELECT 1 FROM entities WHERE id = ?")
            .bind(&id)
            .fetch_optional(&mut *personal)
            .await?;
        if exists.is_none() {
            report
                .counts
                .add(&purge_outputs_side(outputs, &id, &mut |_| Ok(())).await?);
            report.orphans += 1;
        }
    }
    Ok(report)
}

/// Remove one catalogued field of an owned device for good: every row of it, the
/// active value and the superseded ones (the device sheet's "clear this fact").
/// Returns how many rows it removed, 0 for a field that was never set. The field
/// must be in the device catalogue and the entity must be this Focus's own active
/// one. Not consent-gated and not latch-gated: removing data is the user's to do.
pub async fn clear_owned_fact(
    personal: &mut SqliteConnection,
    focus: &str,
    entity_id: &str,
    field: &str,
) -> Result<i64, PurgeError> {
    check_focus(focus)?;
    check_entity_id(entity_id)?;
    let catalogue = crate::conductor::device_fields::registry().map_err(PurgeError::Validation)?;
    let def = catalogue
        .field(field.trim())
        .ok_or_else(|| PurgeError::Validation("unknown_field".to_owned()))?;
    crate::persistence::owned_entity_store::get_owned_active(personal, focus, entity_id)
        .await
        .map_err(|_| PurgeError::NotFound)?;
    let done = sqlx::query("DELETE FROM entity_facts WHERE entity_id = ? AND field_name = ?")
        .bind(entity_id)
        .bind(&def.name)
        .execute(&mut *personal)
        .await?;
    Ok(done.rows_affected() as i64)
}

// ---------------------------------------------------------------------------
// Run control and the persona-level entry points
// ---------------------------------------------------------------------------

/// Ask the Focus's live runs to stop and wait, up to `wait`, for them to end. A run
/// that ignores the stop leaves this an Err and the caller does nothing else.
pub async fn stop_focus_runs(
    user_id: &str,
    persona_id: &str,
    focus: &str,
    wait: Duration,
) -> Result<(), PurgeError> {
    check_focus(focus)?;
    crate::auth::writer_gate::request_stop_for_focus(user_id, persona_id, focus);
    if crate::auth::writer_gate::wait_focus_runs_ended(user_id, persona_id, focus, wait).await {
        Ok(())
    } else {
        Err(PurgeError::RunsStillRunning)
    }
}

/// Cancel every non-terminal run of the Focus in this persona (the parked ones: a
/// live one has ended through its stop). Cancel clears the run's snapshots and its
/// kept chat window. Returns how many runs it cancelled.
pub async fn cancel_parked_runs(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
    focus: &str,
) -> Result<usize, PurgeError> {
    check_focus(focus)?;
    let ids: Vec<String> = {
        let mut conn = open_outputs_db(user_id, persona_id, key_hex).await?;
        sqlx::query_scalar(
            "SELECT id FROM focus_runs
             WHERE focus_id = ? AND status NOT IN ('complete', 'cancelled', 'failed')
             ORDER BY id",
        )
        .bind(focus)
        .fetch_all(&mut conn)
        .await?
    };
    for id in &ids {
        output_store::cancel_focus_run(user_id, persona_id, key_hex, id).await?;
    }
    Ok(ids.len())
}

/// Purge one device for the user: stop the Focus's live runs (refusing, with
/// nothing deleted, if one ignores the stop), then purge it.
pub async fn purge_device(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
    focus: &str,
    entity_id: &str,
    wait: Duration,
) -> Result<PurgeCounts, PurgeError> {
    stop_focus_runs(user_id, persona_id, focus, wait).await?;
    let mut personal = open_personal_db(user_id, persona_id, key_hex).await?;
    let mut outputs = open_outputs_db(user_id, persona_id, key_hex).await?;
    purge_entity(&mut personal, &mut outputs, focus, entity_id).await
}

/// Purge EVERY device the Focus owns in this persona (a withdrawal that deletes),
/// whatever its status: stop the live runs (refusing, with nothing deleted, if one
/// ignores the stop), cancel the parked runs, finish any half-done purge, purge each
/// device and sweep the orphans. Returns the totals.
pub async fn purge_all_devices(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
    focus: &str,
    wait: Duration,
) -> Result<PurgeCounts, PurgeError> {
    stop_focus_runs(user_id, persona_id, focus, wait).await?;
    cancel_parked_runs(user_id, persona_id, key_hex, focus).await?;
    let mut personal = open_personal_db(user_id, persona_id, key_hex).await?;
    let mut outputs = open_outputs_db(user_id, persona_id, key_hex).await?;

    let mut totals = reconcile(&mut personal, &mut outputs).await?.counts;
    let owned = list_entities_conn(
        &mut personal,
        &EntityFilter {
            owner: OwnerFilter::Focus(focus.to_owned()),
            ..Default::default()
        },
    )
    .await?;
    for e in owned {
        totals.add(&purge_entity(&mut personal, &mut outputs, focus, &e.id).await?);
    }
    totals.add(&reconcile(&mut personal, &mut outputs).await?.counts);
    Ok(totals)
}

fn persona_file(user_id: &str, persona_id: &str, name: &str) -> std::path::PathBuf {
    crate::persistence::migrations::get_data_root()
        .join("users")
        .join(user_id)
        .join("personas")
        .join(persona_id)
        .join(name)
}

/// reconcile for one persona, opening its two databases only if both files exist
/// (a persona that never ran a Focus has no outputs.db, and a reconcile must not
/// create one).
pub async fn reconcile_persona(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
) -> Result<ReconcileReport, PurgeError> {
    if !persona_file(user_id, persona_id, "personal.db").exists()
        || !persona_file(user_id, persona_id, "outputs.db").exists()
    {
        return Ok(ReconcileReport::default());
    }
    let mut personal = open_personal_db(user_id, persona_id, key_hex).await?;
    let mut outputs = open_outputs_db(user_id, persona_id, key_hex).await?;
    reconcile(&mut personal, &mut outputs).await
}

/// reconcile for every persona of the signed-in user. Best effort: a persona that
/// fails is logged and the rest go on. Run at login in a supervised task.
pub async fn reconcile_user(pool: &sqlx::SqlitePool, user_id: &str, key_hex: &str) {
    let persona_ids =
        match crate::persistence::persona_store::list_persona_ids_for_user(pool, user_id).await {
            Ok(ids) => ids,
            Err(e) => {
                log::warn!("entity_purge: could not list personas for the reconcile: {e}");
                return;
            }
        };
    for persona_id in persona_ids {
        match reconcile_persona(user_id, &persona_id, key_hex).await {
            Ok(r) if r.resumed + r.orphans > 0 => log::info!(
                "entity_purge: reconcile for a persona finished {} half-done purge(s) and \
                 removed {} orphan id(s)",
                r.resumed,
                r.orphans
            ),
            Ok(_) => {}
            Err(e) => log::warn!("entity_purge: reconcile for a persona failed: {e}"),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests: real encrypted personal.db and outputs.db files
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persistence::case_store::{self, NewCase, NewConfigDelta, NewStrategy};
    use crate::persistence::owned_entity_store as owned;
    use crate::persistence::session_store::{put_session_variable, SessionWrite};

    const KEY_HEX: &str = "deadbeef00112233445566778899aabbccddeeff00112233445566778899aabb";
    const USER: &str = "u-purge";
    const PERSONA: &str = "p-purge";
    const FOCUS: &str = "tech-support";
    const OTHER: &str = "other-focus";
    const CHAT1: &str = "chat-1";
    const CHAT2: &str = "chat-2";

    struct TestRoot {
        _lock: tokio::sync::MutexGuard<'static, ()>,
        saved: Option<String>,
        _dir: tempfile::TempDir,
    }

    impl TestRoot {
        async fn new() -> Self {
            let lock = crate::test_support::ENV_MUTEX.lock().await;
            let saved = std::env::var("QR_DATA_ROOT").ok();
            let dir = tempfile::tempdir().expect("tempdir");
            std::env::set_var("QR_DATA_ROOT", dir.path());
            TestRoot {
                _lock: lock,
                saved,
                _dir: dir,
            }
        }
    }

    impl Drop for TestRoot {
        fn drop(&mut self) {
            match &self.saved {
                Some(v) => std::env::set_var("QR_DATA_ROOT", v),
                None => std::env::remove_var("QR_DATA_ROOT"),
            }
        }
    }

    struct Db {
        personal: SqliteConnection,
        outputs: SqliteConnection,
    }

    async fn dbs() -> Db {
        Db {
            personal: open_personal_db(USER, PERSONA, KEY_HEX).await.unwrap(),
            outputs: open_outputs_db(USER, PERSONA, KEY_HEX).await.unwrap(),
        }
    }

    /// Two owned devices A and B, with the data the purge must and must not remove:
    ///   case1  primary A, one strategy, affected link to B     -> goes with A
    ///   case2  primary B, affected link to A                   -> stays, link goes
    ///   case3  primary B only                                  -> untouched
    ///   case4  primary A of ANOTHER Focus (an unscoped read)   -> goes with A
    ///   d1 [A], d2 [A, B], d3 [B]                              -> d1 goes, d2 loses a link
    ///   active_case_id: chat1 -> case1, chat2 -> case3, plus locked_intent
    ///   A has a superseded fact; B has one fact.
    struct World {
        a: String,
        b: String,
    }

    async fn world(db: &mut Db) -> World {
        let a = owned::create_subject(&mut db.personal, FOCUS, PERSONA, "Router", None, "r:a")
            .await
            .unwrap();
        let b = owned::create_subject(&mut db.personal, FOCUS, PERSONA, "Modem", None, "r:b")
            .await
            .unwrap();
        for (field, value) in [("firmware_version", "1.0"), ("firmware_version", "2.0")] {
            owned::put_owned_fact(&mut db.personal, FOCUS, PERSONA, &a, field, value)
                .await
                .unwrap();
        }
        owned::put_owned_fact(
            &mut db.personal,
            FOCUS,
            PERSONA,
            &a,
            "serial_number",
            "SN-A",
        )
        .await
        .unwrap();
        owned::put_owned_fact(&mut db.personal, FOCUS, PERSONA, &b, "make", "Arris")
            .await
            .unwrap();

        let o = &mut db.outputs;
        for (case_id, focus, primary) in [
            ("case1", FOCUS, a.as_str()),
            ("case2", FOCUS, b.as_str()),
            ("case3", FOCUS, b.as_str()),
            ("case4", OTHER, a.as_str()),
        ] {
            case_store::open_case(
                o,
                &NewCase {
                    case_id,
                    focus_id: focus,
                    context_key: CHAT1,
                    primary_entity_id: primary,
                    subject_label: "label",
                    symptoms: "symptoms",
                    opened_by_run: "run-1",
                },
            )
            .await
            .unwrap();
        }
        case_store::link_case_entity(o, CHAT1, "case1", &b)
            .await
            .unwrap();
        case_store::link_case_entity(o, CHAT1, "case2", &a)
            .await
            .unwrap();
        case_store::append_case_strategy(
            o,
            CHAT1,
            &NewStrategy {
                case_id: "case1",
                source_run_id: "run-1",
                source_action_id: "log",
                source_attempt: 0,
                strategy: "restart",
                outcome: "no change",
                user_signal: None,
                rung_at: Some(1),
            },
        )
        .await
        .unwrap();
        case_store::append_case_strategy(
            o,
            CHAT1,
            &NewStrategy {
                case_id: "case3",
                source_run_id: "run-1",
                source_action_id: "log",
                source_attempt: 0,
                strategy: "other",
                outcome: "fixed",
                user_signal: None,
                rung_at: Some(1),
            },
        )
        .await
        .unwrap();
        for (delta_id, entities) in [
            ("d1", vec![a.as_str()]),
            ("d2", vec![a.as_str(), b.as_str()]),
            ("d3", vec![b.as_str()]),
        ] {
            case_store::record_config_delta(
                o,
                &NewConfigDelta {
                    delta_id,
                    focus_id: FOCUS,
                    source_case_id: (delta_id == "d1").then_some("case1"),
                    context_key: CHAT1,
                    session_reference: CHAT1,
                    description: "changed DNS",
                    relevance_tags: &[],
                    entity_ids: &entities,
                    changed_at: None,
                    recorded_by_run: "run-1",
                },
            )
            .await
            .unwrap();
        }
        for (ctx, name, value) in [
            (CHAT1, "active_case_id", "case1"),
            (CHAT2, "active_case_id", "case3"),
            (CHAT1, "locked_intent", "device_library"),
        ] {
            put_session_variable(
                o,
                &SessionWrite {
                    context_key: ctx,
                    focus_id: FOCUS,
                    name,
                    value,
                    written_by_run: "run-1",
                    expected_version: None,
                },
            )
            .await
            .unwrap();
        }
        World { a, b }
    }

    async fn n(conn: &mut SqliteConnection, sql: &str) -> i64 {
        sqlx::query_scalar(sql).fetch_one(&mut *conn).await.unwrap()
    }

    #[derive(Debug, PartialEq, Eq)]
    struct Shape {
        entities: i64,
        facts: i64,
        cases: i64,
        links: i64,
        strategies: i64,
        deltas: i64,
        delta_links: i64,
        sessions: i64,
    }

    async fn shape(db: &mut Db) -> Shape {
        Shape {
            entities: n(&mut db.personal, "SELECT COUNT(*) FROM entities").await,
            facts: n(&mut db.personal, "SELECT COUNT(*) FROM entity_facts").await,
            cases: n(&mut db.outputs, "SELECT COUNT(*) FROM case_records").await,
            links: n(&mut db.outputs, "SELECT COUNT(*) FROM case_entities").await,
            strategies: n(&mut db.outputs, "SELECT COUNT(*) FROM case_strategies").await,
            deltas: n(&mut db.outputs, "SELECT COUNT(*) FROM config_deltas").await,
            delta_links: n(
                &mut db.outputs,
                "SELECT COUNT(*) FROM config_delta_entities",
            )
            .await,
            sessions: n(&mut db.outputs, "SELECT COUNT(*) FROM session_variables").await,
        }
    }

    async fn ids(conn: &mut SqliteConnection, sql: &str) -> Vec<String> {
        sqlx::query_scalar(sql).fetch_all(&mut *conn).await.unwrap()
    }

    /// Everything the world holds before any purge.
    fn full() -> Shape {
        Shape {
            entities: 2,
            facts: 4, // A: firmware 1.0 (superseded), firmware 2.0, serial; B: make
            cases: 4,
            links: 6, // case1: A+B, case2: B+A, case3: B, case4: A
            strategies: 2,
            deltas: 3,
            delta_links: 4,
            sessions: 3,
        }
    }

    /// What is left after A is gone: case2 and case3 and B, d2 and d3.
    fn after_a() -> Shape {
        Shape {
            entities: 1,
            facts: 1,
            cases: 2,
            links: 2, // case2: B, case3: B
            strategies: 1,
            deltas: 2,
            delta_links: 2, // d2: B, d3: B
            sessions: 2,    // case3 pointer and locked_intent
        }
    }

    async fn assert_after_a(db: &mut Db, w: &World) {
        assert_eq!(shape(db).await, after_a());
        assert_eq!(
            ids(
                &mut db.outputs,
                "SELECT case_id FROM case_records ORDER BY case_id"
            )
            .await,
            ["case2", "case3"]
        );
        assert_eq!(
            ids(
                &mut db.outputs,
                "SELECT delta_id FROM config_deltas ORDER BY delta_id"
            )
            .await,
            ["d2", "d3"]
        );
        assert_eq!(
            ids(
                &mut db.outputs,
                "SELECT name || '=' || value FROM session_variables ORDER BY name, value"
            )
            .await,
            ["active_case_id=case3", "locked_intent=device_library"]
        );
        assert_eq!(
            ids(&mut db.personal, "SELECT id FROM entities").await,
            std::slice::from_ref(&w.b),
            "only the other device is left"
        );
        // d1's source case went with it, d2 and d3 never had one.
        assert_eq!(
            n(
                &mut db.outputs,
                "SELECT COUNT(*) FROM config_deltas WHERE source_case_id IS NOT NULL"
            )
            .await,
            0
        );
    }

    // -- the purge ------------------------------------------------------------

    #[tokio::test]
    async fn a_purge_removes_exactly_what_the_module_header_says_and_nothing_else() {
        let _root = TestRoot::new().await;
        let mut db = dbs().await;
        let w = world(&mut db).await;
        assert_eq!(shape(&mut db).await, full());

        let counts = purge_entity(&mut db.personal, &mut db.outputs, FOCUS, &w.a)
            .await
            .unwrap();
        assert_after_a(&mut db, &w).await;
        assert_eq!(
            counts,
            PurgeCounts {
                cases: 2,        // case1 (primary) and case4 (another Focus's, unscoped)
                strategies: 1,   // case1's
                case_links: 3,   // case1, case2 (affected), case4
                deltas: 1,       // d1
                delta_links: 2,  // d1, d2
                session_refs: 1, // chat1 -> case1
                facts: 3,
                relationships: 0,
                dedup_candidates: 0,
            }
        );
        // B's data is exactly as it was.
        assert_eq!(
            n(
                &mut db.personal,
                "SELECT COUNT(*) FROM entity_facts WHERE field_name = 'make'"
            )
            .await,
            1
        );
    }

    #[tokio::test]
    async fn the_preview_counts_what_the_purge_then_removes() {
        let _root = TestRoot::new().await;
        let mut db = dbs().await;
        let w = world(&mut db).await;
        let before = shape(&mut db).await;
        let preview = preview_purge(&mut db.personal, &mut db.outputs, FOCUS, &w.a)
            .await
            .unwrap();
        assert_eq!(shape(&mut db).await, before, "a preview deletes nothing");
        let done = purge_entity(&mut db.personal, &mut db.outputs, FOCUS, &w.a)
            .await
            .unwrap();
        assert_eq!(preview, done);
    }

    #[tokio::test]
    async fn a_purge_refuses_an_unowned_another_focus_or_unknown_entity_and_deletes_nothing() {
        let _root = TestRoot::new().await;
        let mut db = dbs().await;
        let w = world(&mut db).await;
        let unowned = crate::persistence::entity_store::create_entity_conn(
            &mut db.personal,
            "person",
            "Sam",
            &[],
            None,
            None,
        )
        .await
        .unwrap();
        let before = shape(&mut db).await;
        for (focus, id) in [
            (OTHER, w.a.as_str()),
            (FOCUS, unowned.as_str()),
            (FOCUS, "not-an-entity"),
        ] {
            assert!(matches!(
                purge_entity(&mut db.personal, &mut db.outputs, focus, id).await,
                Err(PurgeError::NotFound)
            ));
            assert!(matches!(
                preview_purge(&mut db.personal, &mut db.outputs, focus, id).await,
                Err(PurgeError::NotFound)
            ));
        }
        assert_eq!(shape(&mut db).await, before);
        // The unowned entity is still active: nothing marked it.
        let status: String = sqlx::query_scalar("SELECT status FROM entities WHERE id = ?")
            .bind(&unowned)
            .fetch_one(&mut db.personal)
            .await
            .unwrap();
        assert_eq!(status, "active");
    }

    #[tokio::test]
    async fn the_personal_step_only_ever_deletes_a_marked_owned_entity() {
        let _root = TestRoot::new().await;
        let mut db = dbs().await;
        let w = world(&mut db).await;
        let unowned = crate::persistence::entity_store::create_entity_conn(
            &mut db.personal,
            "person",
            "Sam",
            &[],
            None,
            None,
        )
        .await
        .unwrap();
        sqlx::query("UPDATE entities SET status = 'user_deleted' WHERE id = ?")
            .bind(&unowned)
            .execute(&mut db.personal)
            .await
            .unwrap();
        // An active owned entity and an unowned marked one are both left alone.
        for id in [&w.a, &unowned] {
            let c = purge_personal_side(&mut db.personal, id, &mut |_| Ok(()))
                .await
                .unwrap();
            assert_eq!(c, PurgeCounts::default());
        }
        assert_eq!(
            n(&mut db.personal, "SELECT COUNT(*) FROM entities").await,
            3
        );
    }

    #[tokio::test]
    async fn an_archived_device_can_be_purged_and_a_marked_one_is_invisible_to_the_writers() {
        let _root = TestRoot::new().await;
        let mut db = dbs().await;
        let w = world(&mut db).await;
        sqlx::query("UPDATE entities SET status = 'archived' WHERE id = ?")
            .bind(&w.a)
            .execute(&mut db.personal)
            .await
            .unwrap();
        // Marking makes it invisible to the owned-entity layer at once.
        mark_for_purge(&mut db.personal, FOCUS, &w.a).await.unwrap();
        assert!(matches!(
            owned::get_owned_active(&mut db.personal, FOCUS, &w.a).await,
            Err(owned::OwnedEntityError::NotFound)
        ));
        assert!(owned::list_owned_active(&mut db.personal, FOCUS)
            .await
            .unwrap()
            .iter()
            .all(|e| e.id != w.a));
        purge_entity(&mut db.personal, &mut db.outputs, FOCUS, &w.a)
            .await
            .unwrap();
        assert_after_a(&mut db, &w).await;
    }

    // -- failure injection at every step boundary -----------------------------

    async fn interrupted_at(db: &mut Db, w: &World, step: PurgeStep) {
        let mut hook = move |s: PurgeStep| {
            if s == step {
                Err("injected".to_owned())
            } else {
                Ok(())
            }
        };
        let err = purge_entity_with(&mut db.personal, &mut db.outputs, FOCUS, &w.a, &mut hook)
            .await
            .unwrap_err();
        assert!(
            matches!(&err, PurgeError::Interrupted(m) if m == "injected"),
            "{step:?}: {err}"
        );
    }

    async fn status_of(db: &mut Db, id: &str) -> Option<String> {
        sqlx::query_scalar("SELECT status FROM entities WHERE id = ?")
            .bind(id)
            .fetch_optional(&mut db.personal)
            .await
            .unwrap()
    }

    /// After a failure at `step` the state is what that boundary promises, a second
    /// reconcile finds nothing more, and the finished state is exactly a full purge.
    async fn recovers_from(step: PurgeStep) -> Shape {
        let _root = TestRoot::new().await;
        let mut db = dbs().await;
        let w = world(&mut db).await;
        interrupted_at(&mut db, &w, step).await;
        let stopped = shape(&mut db).await;

        // The entity is marked at every boundary: it is the commit point, and the
        // writers and the device list no longer see it.
        assert_eq!(
            status_of(&mut db, &w.a).await.as_deref(),
            Some("user_deleted")
        );
        assert!(matches!(
            owned::get_owned_active(&mut db.personal, FOCUS, &w.a).await,
            Err(owned::OwnedEntityError::NotFound)
        ));

        // RECONCILE finds the marked entity and finishes the purge.
        let r = reconcile(&mut db.personal, &mut db.outputs).await.unwrap();
        assert_eq!(r.resumed, 1, "{step:?}");
        assert_after_a(&mut db, &w).await;
        // And it is idempotent: nothing left to find.
        let again = reconcile(&mut db.personal, &mut db.outputs).await.unwrap();
        assert_eq!(again, ReconcileReport::default(), "{step:?}");
        stopped
    }

    #[tokio::test]
    async fn a_stop_right_after_the_mark_changes_nothing_else_and_reconcile_finishes() {
        let stopped = recovers_from(PurgeStep::AfterMark).await;
        assert_eq!(stopped, full(), "only the mark was written");
    }

    #[tokio::test]
    async fn a_failure_inside_the_outputs_transaction_rolls_it_back_and_reconcile_finishes() {
        let stopped = recovers_from(PurgeStep::InOutputsTransaction).await;
        assert_eq!(stopped, full(), "the outputs transaction rolled back whole");
    }

    #[tokio::test]
    async fn a_stop_after_the_outputs_step_leaves_the_entity_and_reconcile_finishes() {
        let stopped = recovers_from(PurgeStep::AfterOutputs).await;
        // outputs.db is purged, personal.db is not yet.
        assert_eq!(
            stopped,
            Shape {
                entities: 2,
                facts: 4,
                ..after_a()
            }
        );
    }

    #[tokio::test]
    async fn a_failure_inside_the_personal_transaction_rolls_it_back_and_reconcile_finishes() {
        let stopped = recovers_from(PurgeStep::InPersonalTransaction).await;
        assert_eq!(
            stopped,
            Shape {
                entities: 2,
                facts: 4,
                ..after_a()
            },
            "outputs purged, the personal transaction rolled back whole"
        );
    }

    #[tokio::test]
    async fn the_user_can_simply_run_the_purge_again_after_a_stop() {
        for step in [
            PurgeStep::AfterMark,
            PurgeStep::InOutputsTransaction,
            PurgeStep::AfterOutputs,
            PurgeStep::InPersonalTransaction,
        ] {
            let _root = TestRoot::new().await;
            let mut db = dbs().await;
            let w = world(&mut db).await;
            interrupted_at(&mut db, &w, step).await;
            let counts = purge_entity(&mut db.personal, &mut db.outputs, FOCUS, &w.a).await;
            assert!(counts.is_ok(), "{step:?}: {counts:?}");
            assert_after_a(&mut db, &w).await;
        }
    }

    // -- the device sheet's reads and a single fact's clear ---------------------

    #[tokio::test]
    async fn the_cases_of_an_entity_include_closed_and_affected_ones_newest_first() {
        let _root = TestRoot::new().await;
        let mut db = dbs().await;
        let w = world(&mut db).await;
        // Distinct open times, and close one.
        for (case, at) in [
            ("case1", "2026-01-01T00:00:00Z"),
            ("case2", "2026-02-01T00:00:00Z"),
            ("case4", "2026-03-01T00:00:00Z"),
        ] {
            sqlx::query("UPDATE case_records SET opened_at = ? WHERE case_id = ?")
                .bind(at)
                .bind(case)
                .execute(&mut db.outputs)
                .await
                .unwrap();
        }
        let ids = |cases: Vec<case_store::CaseRecord>| -> Vec<String> {
            cases.into_iter().map(|c| c.case_id).collect()
        };
        // A: primary of case1 and case4, affected on case2. All Focuses, then one.
        let all = case_store::list_cases_for_entity(&mut db.outputs, &w.a, None, 50)
            .await
            .unwrap();
        assert_eq!(ids(all), ["case4", "case2", "case1"]);
        let mine = case_store::list_cases_for_entity(&mut db.outputs, &w.a, Some(FOCUS), 50)
            .await
            .unwrap();
        assert_eq!(ids(mine), ["case2", "case1"], "case4 is another Focus's");
        let one = case_store::list_cases_for_entity(&mut db.outputs, &w.a, None, 1)
            .await
            .unwrap();
        assert_eq!(ids(one), ["case4"]);
        assert!(
            case_store::list_cases_for_entity(&mut db.outputs, "nobody", None, 50)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn clearing_a_fact_removes_every_version_of_that_field_only() {
        let _root = TestRoot::new().await;
        let mut db = dbs().await;
        let w = world(&mut db).await;
        // A has firmware_version (two versions) and serial_number.
        let removed = clear_owned_fact(&mut db.personal, FOCUS, &w.a, "firmware_version")
            .await
            .unwrap();
        assert_eq!(removed, 2);
        let left: Vec<String> = ids(
            &mut db.personal,
            "SELECT field_name FROM entity_facts WHERE entity_id IN (SELECT id FROM entities WHERE display_name = 'Router')",
        )
        .await;
        assert_eq!(left, ["serial_number"]);
        // Again: nothing to remove, not an error. Padding in the name is trimmed.
        assert_eq!(
            clear_owned_fact(&mut db.personal, FOCUS, &w.a, " firmware_version ")
                .await
                .unwrap(),
            0
        );
        // B's facts are untouched, and a field outside the catalogue, another
        // Focus, an unknown entity and a marked one are all refused.
        assert!(matches!(
            clear_owned_fact(&mut db.personal, FOCUS, &w.a, "wifi_password").await,
            Err(PurgeError::Validation(m)) if m == "unknown_field"
        ));
        assert!(matches!(
            clear_owned_fact(&mut db.personal, OTHER, &w.a, "serial_number").await,
            Err(PurgeError::NotFound)
        ));
        assert!(matches!(
            clear_owned_fact(&mut db.personal, FOCUS, "nobody", "serial_number").await,
            Err(PurgeError::NotFound)
        ));
        mark_for_purge(&mut db.personal, FOCUS, &w.a).await.unwrap();
        assert!(matches!(
            clear_owned_fact(&mut db.personal, FOCUS, &w.a, "serial_number").await,
            Err(PurgeError::NotFound)
        ));
        assert_eq!(
            n(
                &mut db.personal,
                "SELECT COUNT(*) FROM entity_facts WHERE field_name = 'make'"
            )
            .await,
            1
        );
    }

    // -- reconcile: orphans ----------------------------------------------------

    #[tokio::test]
    async fn reconcile_removes_the_rows_of_an_entity_that_no_longer_exists() {
        let _root = TestRoot::new().await;
        let mut db = dbs().await;
        let w = world(&mut db).await;
        // A device purged completely, then a straggler that named it just before:
        // a case and a delta that point at an id with no entity row.
        purge_entity(&mut db.personal, &mut db.outputs, FOCUS, &w.a)
            .await
            .unwrap();
        case_store::open_case(
            &mut db.outputs,
            &NewCase {
                case_id: "straggler",
                focus_id: FOCUS,
                context_key: CHAT1,
                primary_entity_id: &w.a,
                subject_label: "x",
                symptoms: "x",
                opened_by_run: "run-9",
            },
        )
        .await
        .unwrap();
        case_store::record_config_delta(
            &mut db.outputs,
            &NewConfigDelta {
                delta_id: "straggler-delta",
                focus_id: FOCUS,
                source_case_id: None,
                context_key: CHAT1,
                session_reference: CHAT1,
                description: "x",
                relevance_tags: &[],
                entity_ids: &[&w.a],
                changed_at: None,
                recorded_by_run: "run-9",
            },
        )
        .await
        .unwrap();
        put_session_variable(
            &mut db.outputs,
            &SessionWrite {
                context_key: "chat-3",
                focus_id: FOCUS,
                name: "active_case_id",
                value: "straggler",
                written_by_run: "run-9",
                expected_version: None,
            },
        )
        .await
        .unwrap();

        let r = reconcile(&mut db.personal, &mut db.outputs).await.unwrap();
        assert_eq!((r.resumed, r.orphans), (0, 1));
        assert_after_a(&mut db, &w).await;
        assert_eq!(
            reconcile(&mut db.personal, &mut db.outputs).await.unwrap(),
            ReconcileReport::default()
        );
    }

    #[tokio::test]
    async fn reconcile_never_touches_a_live_entity_owned_or_not() {
        let _root = TestRoot::new().await;
        let mut db = dbs().await;
        let w = world(&mut db).await;
        // An UNOWNED entity that cases name (another Focus's own kind of record).
        let unowned = crate::persistence::entity_store::create_entity_conn(
            &mut db.personal,
            "person",
            "Sam",
            &[],
            None,
            None,
        )
        .await
        .unwrap();
        case_store::open_case(
            &mut db.outputs,
            &NewCase {
                case_id: "casex",
                focus_id: OTHER,
                context_key: CHAT1,
                primary_entity_id: &unowned,
                subject_label: "x",
                symptoms: "x",
                opened_by_run: "run-9",
            },
        )
        .await
        .unwrap();
        let before = shape(&mut db).await;
        let r = reconcile(&mut db.personal, &mut db.outputs).await.unwrap();
        assert_eq!(r, ReconcileReport::default());
        assert_eq!(shape(&mut db).await, before);
        assert_eq!(status_of(&mut db, &w.a).await.as_deref(), Some("active"));
    }

    #[tokio::test]
    async fn reconcile_persona_opens_nothing_for_a_persona_without_files() {
        let _root = TestRoot::new().await;
        let r = reconcile_persona("nobody", "no-persona", KEY_HEX)
            .await
            .unwrap();
        assert_eq!(r, ReconcileReport::default());
        let dir = crate::persistence::migrations::get_data_root().join("users");
        assert!(
            !dir.join("nobody").exists(),
            "no file or directory was created"
        );
    }

    #[tokio::test]
    async fn reconcile_persona_finishes_a_half_done_purge_through_real_files() {
        let _root = TestRoot::new().await;
        let w = {
            let mut db = dbs().await;
            let w = world(&mut db).await;
            interrupted_at(&mut db, &w, PurgeStep::AfterOutputs).await;
            w
        };
        let r = reconcile_persona(USER, PERSONA, KEY_HEX).await.unwrap();
        assert_eq!(r.resumed, 1);
        let mut db = dbs().await;
        assert_after_a(&mut db, &w).await;
    }

    #[tokio::test]
    async fn reconcile_user_finishes_a_half_done_purge_for_every_persona_of_the_user() {
        use crate::auth::kdf;
        let _root = TestRoot::new().await;
        crate::persistence::migrations::migrate_shared_db()
            .await
            .unwrap();
        let pool =
            sqlx::SqlitePool::connect_with(crate::providers::utils::connect_options_unencrypted(
                &crate::providers::utils::db_path_shared(),
            ))
            .await
            .unwrap();
        let user_id = uuid::Uuid::new_v4().to_string();
        let master_key = [0x66u8; kdf::MASTER_KEY_LEN];
        let (_, public) =
            crate::auth::sharing_keypair::derive_sharing_keypair(&master_key, &user_id, 1);
        let key_hex: String = master_key.iter().map(|b| format!("{b:02x}")).collect();
        crate::auth::user_store::create_user(
            &pool,
            &user_id,
            "Alice",
            "user",
            false,
            b"test-salt",
            1024,
            1,
            1,
            public.as_bytes(),
        )
        .await
        .unwrap();
        let persona_id = uuid::Uuid::new_v4().to_string();
        crate::persistence::persona_store::create_persona(
            &pool,
            &persona_id,
            "P",
            "personal",
            &user_id,
            &key_hex,
            None,
        )
        .await
        .unwrap();

        let mut db = Db {
            personal: open_personal_db(&user_id, &persona_id, &key_hex)
                .await
                .unwrap(),
            outputs: open_outputs_db(&user_id, &persona_id, &key_hex)
                .await
                .unwrap(),
        };
        let w = world(&mut db).await;
        interrupted_at(&mut db, &w, PurgeStep::AfterOutputs).await;

        reconcile_user(&pool, &user_id, &key_hex).await;
        assert_after_a(&mut db, &w).await;
    }

    // -- run control -----------------------------------------------------------

    use crate::auth::writer_gate::{acquire, Kind};

    #[tokio::test]
    async fn a_run_that_ignores_the_stop_makes_the_purge_refuse_with_nothing_changed() {
        let _root = TestRoot::new().await;
        let mut db = dbs().await;
        let w = world(&mut db).await;
        let before = shape(&mut db).await;

        // A live run of this Persona and Focus that never looks at its stop token.
        let user = USER;
        let stubborn = acquire(user, Kind::FocusRun).unwrap();
        stubborn.bind_focus(PERSONA, FOCUS);
        let err = purge_device(
            user,
            PERSONA,
            KEY_HEX,
            FOCUS,
            &w.a,
            Duration::from_millis(150),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, PurgeError::RunsStillRunning), "{err}");
        // The stop was requested, and nothing was marked, deleted or changed.
        assert!(stubborn.stop_token().is_cancelled());
        assert_eq!(shape(&mut db).await, before);
        assert_eq!(status_of(&mut db, &w.a).await.as_deref(), Some("active"));

        // The same for purge_all_devices.
        let err = purge_all_devices(user, PERSONA, KEY_HEX, FOCUS, Duration::from_millis(150))
            .await
            .unwrap_err();
        assert!(matches!(err, PurgeError::RunsStillRunning));
        assert_eq!(shape(&mut db).await, before);
        drop(stubborn);
    }

    #[tokio::test]
    async fn a_run_that_honours_the_stop_lets_the_purge_proceed() {
        let _root = TestRoot::new().await;
        let mut db = dbs().await;
        let w = world(&mut db).await;
        let user = USER;
        let lease = acquire(user, Kind::FocusRun).unwrap();
        lease.bind_focus(PERSONA, FOCUS);
        // A run of ANOTHER Focus is neither stopped nor waited for.
        let other = acquire(user, Kind::FocusRun).unwrap();
        other.bind_focus(PERSONA, OTHER);
        let token = lease.stop_token().clone();
        let ender = tokio::spawn(async move {
            token.cancelled().await;
            drop(lease);
        });
        purge_device(user, PERSONA, KEY_HEX, FOCUS, &w.a, Duration::from_secs(10))
            .await
            .unwrap();
        ender.await.unwrap();
        assert!(!other.stop_token().is_cancelled());
        assert_after_a(&mut db, &w).await;
    }

    async fn park_run(db: &mut Db, id: &str, focus: &str, status: &str) {
        sqlx::query(
            "INSERT INTO focus_runs (id, focus_id, status, started_at, user_input)
             VALUES (?, ?, ?, 't', 'the chat window naming the Router')",
        )
        .bind(id)
        .bind(focus)
        .bind(status)
        .execute(&mut db.outputs)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO focus_run_snapshots
             (id, focus_run_id, action_id, phase, checkpoint_hash, created_at)
             VALUES (?, ?, 'a', 1, 'h', 't')",
        )
        .bind(format!("snap-{id}"))
        .bind(id)
        .execute(&mut db.outputs)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn purge_all_devices_removes_every_device_and_cancels_the_parked_runs_of_the_focus() {
        let _root = TestRoot::new().await;
        let mut db = dbs().await;
        let w = world(&mut db).await;
        // An archived third device, another Focus's device, and runs.
        let c = owned::create_subject(&mut db.personal, FOCUS, PERSONA, "Camera", None, "r:c")
            .await
            .unwrap();
        sqlx::query("UPDATE entities SET status = 'archived' WHERE id = ?")
            .bind(&c)
            .execute(&mut db.personal)
            .await
            .unwrap();
        let theirs = owned::create_subject(&mut db.personal, OTHER, PERSONA, "Garage", None, "r:t")
            .await
            .unwrap();
        park_run(&mut db, "run-parked", FOCUS, "awaiting_user").await;
        park_run(&mut db, "run-sched", FOCUS, "awaiting_schedule").await;
        park_run(&mut db, "run-other", OTHER, "awaiting_user").await;
        park_run(&mut db, "run-done", FOCUS, "complete").await;
        // A half-done purge of A that purge_all must also finish.
        interrupted_at(&mut db, &w, PurgeStep::AfterMark).await;

        let totals = purge_all_devices(USER, PERSONA, KEY_HEX, FOCUS, Duration::from_millis(100))
            .await
            .unwrap();
        assert!(totals.facts >= 4 && totals.cases >= 3, "{totals:?}");

        // Every device of the Focus is gone, in any status, and the other Focus's
        // device and data are not touched.
        assert_eq!(
            ids(&mut db.personal, "SELECT id FROM entities").await,
            [theirs],
        );
        let s = shape(&mut db).await;
        assert_eq!((s.cases, s.deltas, s.links, s.delta_links), (0, 0, 0, 0));
        assert_eq!(
            ids(&mut db.outputs, "SELECT name FROM session_variables").await,
            ["locked_intent"],
            "only the pointer to a deleted case went"
        );

        // Parked runs of the Focus are cancelled and keep no snapshot or chat copy.
        for run in ["run-parked", "run-sched"] {
            let (status, input): (String, Option<String>) =
                sqlx::query_as("SELECT status, user_input FROM focus_runs WHERE id = ?")
                    .bind(run)
                    .fetch_one(&mut db.outputs)
                    .await
                    .unwrap();
            assert_eq!((status.as_str(), input), ("cancelled", None), "{run}");
            assert_eq!(
                n(
                    &mut db.outputs,
                    &format!(
                        "SELECT COUNT(*) FROM focus_run_snapshots WHERE focus_run_id = '{run}'"
                    )
                )
                .await,
                0
            );
        }
        // Another Focus's run and a finished run are left as they were.
        let other: (String, Option<String>) =
            sqlx::query_as("SELECT status, user_input FROM focus_runs WHERE id = 'run-other'")
                .fetch_one(&mut db.outputs)
                .await
                .unwrap();
        assert_eq!(other.0, "awaiting_user");
        assert!(other.1.is_some());
        let done: String =
            sqlx::query_scalar("SELECT status FROM focus_runs WHERE id = 'run-done'")
                .fetch_one(&mut db.outputs)
                .await
                .unwrap();
        assert_eq!(done, "complete");
    }

    #[tokio::test]
    async fn a_single_device_purge_leaves_parked_runs_alone() {
        let _root = TestRoot::new().await;
        let mut db = dbs().await;
        let w = world(&mut db).await;
        park_run(&mut db, "run-parked", FOCUS, "awaiting_user").await;
        purge_device(
            USER,
            PERSONA,
            KEY_HEX,
            FOCUS,
            &w.a,
            Duration::from_millis(100),
        )
        .await
        .unwrap();
        let status: String =
            sqlx::query_scalar("SELECT status FROM focus_runs WHERE id = 'run-parked'")
                .fetch_one(&mut db.outputs)
                .await
                .unwrap();
        assert_eq!(status, "awaiting_user");
    }

    // -- the one-module-deletes scan -------------------------------------------

    fn rust_files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                rust_files(&path, out);
            } else if path.extension().is_some_and(|e| e == "rs") {
                out.push(path);
            }
        }
    }

    /// Only entity_purge.rs deletes an entity, its facts, a case, a case link, a
    /// strategy or a configuration change. (session_store.rs and case_store.rs keep
    /// their own no-delete scans in session_case_tests.rs, which this module does
    /// not weaken: it is a third file.)
    #[test]
    fn only_entity_purge_deletes_the_entity_case_and_delta_tables() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        rust_files(&root, &mut files);
        assert!(files.len() > 50);
        let tables = [
            "entities",
            "entity_facts",
            "entity_relationships",
            "case_records",
            "case_entities",
            "case_strategies",
            "config_deltas",
            "config_delta_entities",
        ];
        let mut deleters = 0;
        for file in files {
            let rel = file
                .strip_prefix(&root)
                .unwrap()
                .to_string_lossy()
                .to_string();
            let text = std::fs::read_to_string(&file).unwrap();
            let production = text.split("\n#[cfg(test)]").next().unwrap();
            let own = rel == "persistence/entity_purge.rs";
            let is_test_file = rel.ends_with("_tests.rs") || rel == "test_support.rs";
            if is_test_file {
                continue;
            }
            for line in production.lines() {
                let l = line.to_lowercase();
                if line.trim_start().starts_with("//") || !l.contains("delete from") {
                    continue;
                }
                let hit = tables.iter().any(|t| {
                    let needle = format!("delete from {t}");
                    l.find(&needle).is_some_and(|i| {
                        // a whole word: the next character is not part of a longer name
                        !l[i + needle.len()..]
                            .chars()
                            .next()
                            .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
                    })
                });
                if hit {
                    assert!(own, "{rel} deletes one of the entity tables: {line}");
                    deleters += 1;
                }
            }
        }
        assert!(
            deleters >= 7,
            "the scan did not see the purge's own deletes"
        );
    }

    /// The purge module touches neither the consent state nor the latch and creates
    /// no entity.
    #[test]
    fn the_purge_module_never_creates_an_entity_or_touches_consent() {
        let src = include_str!("entity_purge.rs");
        let prod = src.split("\n#[cfg(test)]").next().unwrap();
        for forbidden in [
            "insert into",
            "consent_audit",
            "set_collection_consent",
            "write_mode",
            "latch",
        ] {
            let hits: Vec<&str> = prod
                .lines()
                .filter(|l| !l.trim_start().starts_with("//"))
                .filter(|l| l.to_lowercase().contains(forbidden))
                .collect();
            assert!(hits.is_empty(), "`{forbidden}`: {hits:?}");
        }
    }
}
