// src-tauri/src/conductor/entity_writer.rs
//
// items.id=891 stage 3 (decisions.id=507, 509, 940 ruling 2): the HANDLERS of the
// two Focus-owned entity writers, entity_find_or_create and entity_fact_put, and
// the check that every entity id any writer names is this Focus's own. Called
// only from store_writer::run_store_op, after its consent check and its context
// key, with the persona's outputs.db (for the one-time latch) and personal.db
// (where the entity lives).
//
// WHAT A HANDLER MAY CALL: persistence::owned_entity_store (typed, owner-scoped,
// no delete) and persistence::handles::resolve_entity_handle, plus the read of the
// latch (session_store::write_mode). It writes no SQL of its own and never
// deletes: only entity_purge (stage 4) deletes an owned entity, and a source scan
// pins that this file contains no DELETE.
//
// THE LATCH. Entities live in personal.db and the latch in outputs.db, so one
// transaction cannot cover both. Each entity operation reads the latch FIRST and a
// latched chat is a refusal (RefusedOneTime): a one-time session creates no
// entity (decisions.id=509). An unreadable latch fails the pass closed. The latch
// is read once, before the write, and nothing is undone afterwards: a latch set
// between the read and the write by another pass in the same chat lets that one
// write through, the small window Jason accepted for items.id=891 (the same
// granularity as the consent gate's per-op race, decisions.id=944 R7).
//
// entity_find_or_create resolves, in this order:
//   1. the REPLAY KEY "{run_id}:{action_id}": a re-run of this very row (a retry,
//      a resume) finds the entity it already created and writes nothing.
//   2. a HANDLE, when the Handle slot holds something shaped like an entity
//      handle: resolved store-side among this Focus's own active entities. A
//      handle-shaped value that resolves to nothing is an error (a stale or
//      invented handle), and text that is not handle-shaped (a model saying
//      "none") is not a handle and is ignored, so the row goes on to the name.
//   3. the NAME: an exact normalized match on a display name or alias, so a device
//      named twice is one device.
//   4. otherwise CREATE one, with the optional subject class.
// It publishes the entity id under its store_target. The id is derived from the
// store, not from model text, and is never shown to a model.
//
// entity_fact_put writes one catalogued fact on an entity this pass published.
// The field must be in conductor::device_fields (an unknown field, such as a
// credential, fails the pass closed) and the value must fit it.
//
// EVERY ENTITY ID is verified (verify_entity_arguments) before the case, delta or
// fact writer that names it runs: the entity must exist, be owned by the running
// Focus and be active. LOAD already requires the variable to be an entity id an
// earlier entity_find_or_create row published, so this is the second lock: an id
// a purge removed in the middle of a run, or one that never was an entity, writes
// nothing and fails the pass.
//
// Error text names the slot or the reason, never a value.

use sqlx::SqliteConnection;

use crate::conductor::store_op::{Op, Slot};
use crate::conductor::store_writer::{
    derived_id, done, Args, Done, Outcome, StoreWriteError, WriterContext,
};
use crate::persistence::handles;
use crate::persistence::owned_entity_store::{self, PutOutcome};
use crate::persistence::session_store::{self, LatchError, SessionStoreError, WriteMode};

/// Read the one-time latch. A latched chat is the store's refusal, which
/// run_store_op turns into RefusedOneTime. A latch that cannot be read fails
/// closed.
async fn ensure_chat_open(
    outputs: &mut SqliteConnection,
    key: &str,
) -> Result<(), StoreWriteError> {
    match session_store::write_mode(outputs, key).await {
        Ok(WriteMode::Open) => Ok(()),
        Ok(WriteMode::Suppressed) => Err(StoreWriteError::Session(SessionStoreError::Latch(
            LatchError::Suppressed,
        ))),
        Err(e) => Err(StoreWriteError::Session(SessionStoreError::Latch(e))),
    }
}

/// The entity ids an operation names, in slot order.
fn entity_ids(a: &Args<'_>) -> Result<Vec<String>, StoreWriteError> {
    match a.op.op {
        Op::CaseOpen | Op::CaseLink | Op::DeltaChanged | Op::EntityFactPut => {
            Ok(vec![a.required(Slot::Entity)?.to_owned()])
        }
        Op::DeltaRecord => Ok(a.list_or_one(Slot::Entities)?.unwrap_or_default()),
        _ => Ok(Vec::new()),
    }
}

/// Every entity id the row names is an ACTIVE entity this Focus owns. An id that
/// fails is "no such entity for this Focus": missing, another Focus's, unowned and
/// purge-marked are indistinguishable.
pub(crate) async fn verify_entity_arguments(
    personal: &mut SqliteConnection,
    ctx: &WriterContext<'_>,
    a: &Args<'_>,
) -> Result<(), StoreWriteError> {
    for id in entity_ids(a)? {
        owned_entity_store::get_owned_active(personal, ctx.focus_id, &id).await?;
    }
    Ok(())
}

/// Run entity_find_or_create or entity_fact_put. The caller (run_store_op) has
/// already applied the consent gate and found the context key.
pub(crate) async fn run_entity_op(
    outputs: &mut SqliteConnection,
    personal: &mut SqliteConnection,
    key: &str,
    ctx: &WriterContext<'_>,
    a: &Args<'_>,
) -> Result<Done, StoreWriteError> {
    ensure_chat_open(outputs, key).await?;
    match a.op.op {
        Op::EntityFindOrCreate => find_or_create(personal, ctx, a).await,
        Op::EntityFactPut => fact_put(personal, ctx, a).await,
        _ => Err(StoreWriteError::EntityStoreUnavailable),
    }
}

async fn find_or_create(
    personal: &mut SqliteConnection,
    ctx: &WriterContext<'_>,
    a: &Args<'_>,
) -> Result<Done, StoreWriteError> {
    let label = a.required(Slot::Label)?;
    let class = a.optional(Slot::Class)?;
    let focus = ctx.focus_id;
    let replay_key = derived_id(ctx);

    let (id, outcome) = if let Some(found) =
        owned_entity_store::find_owned_by_replay_key(personal, focus, &replay_key).await?
    {
        (found.id, Outcome::AlreadyApplied)
    } else if let Some(handle) = a
        .optional(Slot::Handle)?
        .filter(|h| handles::looks_like_entity_handle(h))
    {
        (
            handles::resolve_entity_handle(personal, focus, handle).await?,
            Outcome::AlreadyApplied,
        )
    } else if let Some(found) =
        owned_entity_store::find_owned_by_name(personal, focus, label).await?
    {
        (found.id, Outcome::AlreadyApplied)
    } else {
        let id = owned_entity_store::create_subject(
            personal,
            focus,
            ctx.persona_id,
            label,
            class,
            &replay_key,
        )
        .await?;
        (id, Outcome::Applied)
    };

    let mut d = done(outcome);
    d.published = a.op.published_var().map(|v| (v.to_owned(), id));
    Ok(d)
}

async fn fact_put(
    personal: &mut SqliteConnection,
    ctx: &WriterContext<'_>,
    a: &Args<'_>,
) -> Result<Done, StoreWriteError> {
    let out = owned_entity_store::put_owned_fact(
        personal,
        ctx.focus_id,
        ctx.persona_id,
        a.required(Slot::Entity)?,
        a.required(Slot::Field)?,
        a.required(Slot::Value)?,
    )
    .await?;
    Ok(done(match out {
        PutOutcome::Written => Outcome::Applied,
        PutOutcome::Unchanged => Outcome::AlreadyApplied,
    }))
}
