// src-tauri/src/conductor/store_writer.rs
//
// items.id=887 (decisions.id=939 section 4): the HANDLERS of the typed store
// writers (conductor::store_op). One function, run_store_op(), runs one
// store_op row against the persona's outputs.db.
//
// WHAT A HANDLER MAY CALL. Only the typed, latch-gated store functions of
// items.id=881: session_store::{latch_write_suppressed, put_session_variable}
// and case_store::{open_case, update_case, append_case_strategy,
// record_config_delta, set_delta_entity_status}, plus the read-only getters
// those need (get_session_variable, get_case, list_config_deltas_for_entity,
// write_mode). It never writes SQL of its own, so the store layer stays the one
// place that enforces the one-time latch, the version compare-and-set, the
// closed-case rule and the append-only strategy log. A mis-authored row cannot
// leak a write past the latch: the store re-reads the latch inside every write
// transaction.
//
// EVERY WRITE CARRIES THE RUNNING FOCUS. focus_id and context_key come from the
// run (WriterContext), never from a row or a variable:
//   * session variables are keyed (context_key, focus_id, name);
//   * open_case and record_config_delta take the running focus_id;
//   * the stores' case and delta writers do not take a focus_id for an EXISTING
//     row, so this module checks it first: a case or delta that another Focus
//     recorded is "no such case for this Focus" (decisions.id=509: one Focus
//     never uses another's history). focus_id of a row never changes, so the
//     check cannot go stale between the read and the write.
//
// HANDLES (items.id=889). case_select and delta_confirm take a HANDLE, the short
// opaque text derived text shows (persistence::handles), never an id. The handler
// resolves it store-side, scoped to the running focus_id: a handle that is
// malformed, matches nothing of this Focus, or matches more than one writes
// nothing and fails the pass closed (a latched chat still refuses first, like
// every writer). entity_changed and case_link name a case by the session-bound
// active_case_id or a published id, which the store checks belongs to this Focus.
// session_clear removes only locked_intent and active_case_id, never the latch.
//
// NEVER AN EXPLICIT `at`. set_delta_entity_status is always called with
// at = None, so the 90-day clock of decisions.id=510 starts at the moment of the
// write and a row can neither back-date nor push it forward (items.id=881
// review: the clock is deliberately not clamped, so the only safe writer never
// passes one). A source-scan test pins it.
//
// DERIVED IDS. case_open and delta_record choose their idempotency key from the
// run, "{run_id}:{action_id}", never from content: a re-run of the same row (a
// retry, a resume) is Existing and writes nothing. The id is published to later
// rows through the row's store_target (a pass variable, not a TaskTrack step).
//
// OUTCOMES.
//   Applied         the store wrote.
//   AlreadyApplied  the store found the write already there (an idempotent
//                   replay, an identical session value, a duplicate strategy).
//   RefusedOneTime  the chat is a one-time session (the latch is set): the store
//                   refused, NOTHING was written, and the pass goes on. This is
//                   the store's contract working, not a failure: a Focus guards
//                   its writers on write_mode, and a row that was not guarded
//                   still writes nothing. In a latched chat EVERY writer is a
//                   refusal, including one whose arguments would not resolve.
// Every other failure is an Err and FAILS THE PASS CLOSED (lifecycle): an
// unreadable latch, a version conflict, a closed case, an argument that is
// unset, empty, or the wrong shape, a store error. Error text names the slot
// and the variable, never a value (values are user and model content).

use std::collections::HashMap;

use chrono::Utc;
use sqlx::SqliteConnection;

use crate::conductor::bound_spec::{self, ACTIVE_CASE_ID};
use crate::conductor::store_op::{Op, Slot, StoreOp};
use crate::persistence::case_store::{
    self, AppendOutcome, CaseState, CaseStoreError, CaseUpdate, DeltaOutcome, DeltaStatus,
    LinkOutcome, NewCase, NewConfigDelta, NewStrategy, OpenOutcome, ResolutionPath,
};
use crate::persistence::handles::{self, HandleError};
use crate::persistence::session_store::{
    self, LatchError, LatchOutcome, PutOutcome, SessionStoreError, SessionWrite, WriteMode,
};

/// Most entity ids one delta_record row names, and most tags.
pub const MAX_LIST_ITEMS: usize = 32;

/// The running pass, as the writer needs it. All three ids come from the run.
#[derive(Debug, Clone, Copy)]
pub struct WriterContext<'a> {
    /// The chat. None for a run that belongs to no chat: nothing can be written.
    pub context_key: Option<&'a str>,
    pub focus_id: &'a str,
    pub run_id: &'a str,
    pub action_id: &'a str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Applied,
    AlreadyApplied,
    RefusedOneTime,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Done {
    pub outcome: Outcome,
    /// (pass variable, id) when the row published one. Never set for a refusal.
    pub published: Option<(String, String)>,
}

#[derive(Debug, thiserror::Error)]
pub enum StoreWriteError {
    #[error("this run belongs to no chat, so it cannot write to the session or case store")]
    NoContextKey,
    #[error("store writer '{op}': slot '{slot}' names variable '{var}', which is not set")]
    ArgumentUnset {
        op: &'static str,
        slot: &'static str,
        var: String,
    },
    #[error("store writer '{op}': slot '{slot}' is empty")]
    ArgumentEmpty {
        op: &'static str,
        slot: &'static str,
    },
    #[error("store writer '{op}': slot '{slot}' is not usable: {reason}")]
    ArgumentInvalid {
        op: &'static str,
        slot: &'static str,
        reason: &'static str,
    },
    #[error("session variable '{0}' does not accept that value")]
    InvalidSessionValue(String),
    #[error("no such case for this Focus")]
    CaseNotInFocus,
    #[error("no such configuration change for this Focus")]
    DeltaNotInFocus,
    #[error("the id derived for this row is not valid")]
    DerivedIdInvalid,
    #[error("the case has no primary subject entity to key the event on")]
    CaseHasNoEntity,
    #[error("{0}")]
    Handle(#[from] HandleError),
    #[error("{0}")]
    Session(#[from] SessionStoreError),
    #[error("{0}")]
    Case(#[from] CaseStoreError),
    #[error("this persona's store could not be opened: {0}")]
    Open(String),
}

impl StoreWriteError {
    /// True when the store refused because the chat is a one-time session.
    fn is_one_time_refusal(&self) -> bool {
        matches!(
            self,
            StoreWriteError::Session(SessionStoreError::Latch(LatchError::Suppressed))
                | StoreWriteError::Case(CaseStoreError::Latch(LatchError::Suppressed))
        )
    }
}

/// Run one store writer. See the module header for what it may call and for
/// the outcomes. `vars` is the pass's variable view: the TaskTrack's output
/// variables, the ids earlier writers published, and the seeded session names.
pub async fn run_store_op(
    conn: &mut SqliteConnection,
    ctx: &WriterContext<'_>,
    op: &StoreOp,
    vars: &HashMap<String, String>,
) -> Result<Done, StoreWriteError> {
    let key = ctx.context_key.ok_or(StoreWriteError::NoContextKey)?;
    let result = run_inner(conn, key, ctx, op, vars).await;
    let err = match result {
        Ok(done) => return Ok(done),
        Err(e) => e,
    };
    // The latch is not a failure of the write: the chat is one-time.
    if op.op != Op::Latch {
        if err.is_one_time_refusal() {
            return Ok(refused());
        }
        // A latched chat refuses every writer, whatever else is wrong with its
        // arguments. A latch that cannot be read leaves the original error.
        if let Ok(WriteMode::Suppressed) = session_store::write_mode(conn, key).await {
            return Ok(refused());
        }
    }
    Err(err)
}

fn refused() -> Done {
    Done {
        outcome: Outcome::RefusedOneTime,
        published: None,
    }
}

fn done(outcome: Outcome) -> Done {
    Done {
        outcome,
        published: None,
    }
}

async fn run_inner(
    conn: &mut SqliteConnection,
    key: &str,
    ctx: &WriterContext<'_>,
    op: &StoreOp,
    vars: &HashMap<String, String>,
) -> Result<Done, StoreWriteError> {
    let a = Args { op, vars };
    match op.op {
        Op::Latch => {
            let out =
                session_store::latch_write_suppressed(conn, key, ctx.focus_id, ctx.run_id).await?;
            Ok(done(match out {
                LatchOutcome::Set => Outcome::Applied,
                LatchOutcome::AlreadySet => Outcome::AlreadyApplied,
            }))
        }

        Op::SessionPut => {
            let name = op.target.as_deref().unwrap_or("");
            let value = a.required(Slot::Value)?;
            // The value must satisfy the spec of the name it is written under.
            let valid = bound_spec::spec(name).is_some_and(|s| (s.validate)(value));
            if !valid {
                return Err(StoreWriteError::InvalidSessionValue(name.to_owned()));
            }
            // The chat's active case is a case this Focus opened: a variable
            // (model output) cannot point the Focus at another Focus's case.
            if name == ACTIVE_CASE_ID {
                own_case(conn, ctx, value).await?;
            }
            put_session(conn, key, ctx, name, value).await
        }

        Op::CaseSelect => {
            // The handle is resolved HERE, in the store layer, to an open case
            // this Focus opened: nothing a model says can reach another Focus's
            // case, a closed case or an invented id.
            let handle = a.required(Slot::Handle)?;
            let case_id = handles::resolve_case_handle(conn, ctx.focus_id, handle).await?;
            let valid = bound_spec::spec(ACTIVE_CASE_ID).is_some_and(|s| (s.validate)(&case_id));
            if !valid {
                return Err(StoreWriteError::DerivedIdInvalid);
            }
            put_session(conn, key, ctx, ACTIVE_CASE_ID, &case_id).await
        }

        Op::CaseLink => {
            let case = own_case(conn, ctx, a.required(Slot::Case)?).await?;
            let entity = a.required(Slot::Entity)?;
            let out = case_store::link_case_entity(conn, key, &case.case_id, entity).await?;
            Ok(done(match out {
                LinkOutcome::Linked => Outcome::Applied,
                LinkOutcome::AlreadyLinked => Outcome::AlreadyApplied,
            }))
        }

        Op::EntityChanged => {
            // The entity is the case's primary subject, read from the store: a
            // model never names an entity id. Only this Focus's deltas move.
            let case = own_case(conn, ctx, a.required(Slot::Case)?).await?;
            let entity = case
                .primary_entity_id
                .as_deref()
                .ok_or(StoreWriteError::CaseHasNoEntity)?;
            let moved =
                case_store::mark_entity_deltas_changed(conn, key, ctx.focus_id, entity).await?;
            Ok(done(if moved > 0 {
                Outcome::Applied
            } else {
                Outcome::AlreadyApplied
            }))
        }

        Op::SessionClear => {
            let removed = session_store::clear_session_state(conn, key, ctx.focus_id).await?;
            Ok(done(if removed > 0 {
                Outcome::Applied
            } else {
                Outcome::AlreadyApplied
            }))
        }

        Op::CaseOpen => {
            let case_id = derived_id(ctx);
            let valid = bound_spec::spec(ACTIVE_CASE_ID).is_some_and(|s| (s.validate)(&case_id));
            if !valid {
                return Err(StoreWriteError::DerivedIdInvalid);
            }
            let entity = a.required(Slot::Entity)?;
            let label = a.required(Slot::Label)?;
            let symptoms = a.optional(Slot::Symptoms)?.unwrap_or("");
            let out = case_store::open_case(
                conn,
                &NewCase {
                    case_id: &case_id,
                    focus_id: ctx.focus_id,
                    context_key: key,
                    primary_entity_id: entity,
                    subject_label: label,
                    symptoms,
                    opened_by_run: ctx.run_id,
                },
            )
            .await?;
            let mut d = done(match out {
                OpenOutcome::Created(_) => Outcome::Applied,
                OpenOutcome::Existing(_) => Outcome::AlreadyApplied,
            });
            d.published = op.published_var().map(|v| (v.to_owned(), case_id));
            Ok(d)
        }

        Op::CaseUpdate => {
            let case = own_case(conn, ctx, a.required(Slot::Case)?).await?;
            let state = match a.optional(Slot::State)? {
                Some(s) => {
                    Some(CaseState::parse(s).ok_or(a.invalid(Slot::State, "not a case state"))?)
                }
                None => None,
            };
            let rung = match a.optional(Slot::Rung)? {
                Some(s) => Some(
                    s.parse::<i64>()
                        .ok()
                        .filter(|r| (1..=5).contains(r))
                        .ok_or(a.invalid(Slot::Rung, "not a rung from 1 to 5"))?,
                ),
                None => None,
            };
            let path = match a.optional(Slot::ResolutionPath)? {
                Some(s) => Some(
                    ResolutionPath::parse(s)
                        .ok_or(a.invalid(Slot::ResolutionPath, "not a resolution path"))?,
                ),
                None => None,
            };
            let update = CaseUpdate {
                state,
                current_rung: rung,
                resolution_path: path,
                symptoms: a.optional(Slot::Symptoms)?,
                outcome: a.optional(Slot::Outcome)?,
                durable_facts_json: a.optional(Slot::DurableFacts)?,
            };
            // The version just read is the compare-and-set: a concurrent pass
            // that moved the case first makes this a VersionConflict, which
            // fails the pass.
            case_store::update_case(conn, key, &case.case_id, case.version, &update).await?;
            Ok(done(Outcome::Applied))
        }

        Op::StrategyAppend => {
            let case = own_case(conn, ctx, a.required(Slot::Case)?).await?;
            let out = case_store::append_case_strategy(
                conn,
                key,
                &NewStrategy {
                    case_id: &case.case_id,
                    source_run_id: ctx.run_id,
                    source_action_id: ctx.action_id,
                    source_attempt: 0,
                    strategy: a.required(Slot::Strategy)?,
                    outcome: a.required(Slot::Outcome)?,
                    user_signal: a.optional(Slot::UserSignal)?,
                    // The rung the case stood at when the strategy was tried.
                    rung_at: Some(case.current_rung),
                },
            )
            .await?;
            Ok(done(match out {
                AppendOutcome::Appended { .. } => Outcome::Applied,
                AppendOutcome::Duplicate { .. } => Outcome::AlreadyApplied,
            }))
        }

        Op::DeltaRecord => {
            let delta_id = derived_id(ctx);
            let entities = a
                .list(Slot::Entities)?
                .ok_or(StoreWriteError::ArgumentEmpty {
                    op: op.op.as_str(),
                    slot: Slot::Entities.as_str(),
                })?;
            let tags = a.list(Slot::Tags)?.unwrap_or_default();
            let entity_refs: Vec<&str> = entities.iter().map(String::as_str).collect();
            let tag_refs: Vec<&str> = tags.iter().map(String::as_str).collect();
            let source_case = a.optional(Slot::Case)?;
            let out = case_store::record_config_delta(
                conn,
                &NewConfigDelta {
                    delta_id: &delta_id,
                    focus_id: ctx.focus_id,
                    // The store checks that the case exists and is this Focus's.
                    source_case_id: source_case,
                    context_key: key,
                    session_reference: key,
                    description: a.required(Slot::Description)?,
                    relevance_tags: &tag_refs,
                    entity_ids: &entity_refs,
                    changed_at: None,
                    recorded_by_run: ctx.run_id,
                },
            )
            .await?;
            let mut d = done(match out {
                DeltaOutcome::Created => Outcome::Applied,
                DeltaOutcome::Existing => Outcome::AlreadyApplied,
            });
            d.published = op.published_var().map(|v| (v.to_owned(), delta_id));
            Ok(d)
        }

        Op::DeltaConfirm => {
            // By handle: the delta is resolved store-side among the deltas THIS
            // Focus recorded, and confirmed for every entity it names.
            let handle = a.required(Slot::Handle)?;
            let delta = handles::resolve_delta_handle(conn, ctx.focus_id, handle).await?;
            case_store::set_delta_status(conn, key, &delta, DeltaStatus::ConfirmedActive).await?;
            Ok(done(Outcome::Applied))
        }

        Op::DeltaChanged => {
            let delta = a.required(Slot::Delta)?;
            let entity = a.required(Slot::Entity)?;
            // Only this Focus's own deltas: the list is scoped by focus_id.
            let own = case_store::list_config_deltas_for_entity(
                conn,
                entity,
                Some(ctx.focus_id),
                Utc::now(),
            )
            .await?;
            if !own.iter().any(|d| d.delta_id == delta) {
                return Err(StoreWriteError::DeltaNotInFocus);
            }
            case_store::set_delta_entity_status(
                conn,
                key,
                delta,
                entity,
                DeltaStatus::BelievedChanged,
                None,
            )
            .await?;
            Ok(done(Outcome::Applied))
        }
    }
}

/// One session variable write through the typed store: read the current version
/// and put with it as the compare-and-set. The caller has validated the value.
async fn put_session(
    conn: &mut SqliteConnection,
    key: &str,
    ctx: &WriterContext<'_>,
    name: &str,
    value: &str,
) -> Result<Done, StoreWriteError> {
    let current = session_store::get_session_variable(conn, key, ctx.focus_id, name).await?;
    let out = session_store::put_session_variable(
        conn,
        &SessionWrite {
            context_key: key,
            focus_id: ctx.focus_id,
            name,
            value,
            written_by_run: ctx.run_id,
            expected_version: current.map(|v| v.version),
        },
    )
    .await?;
    Ok(done(match out {
        PutOutcome::Created { .. } | PutOutcome::Updated { .. } => Outcome::Applied,
        PutOutcome::Unchanged { .. } => Outcome::AlreadyApplied,
    }))
}

/// The idempotency key of a case or delta this row creates.
fn derived_id(ctx: &WriterContext<'_>) -> String {
    format!("{}:{}", ctx.run_id, ctx.action_id)
}

/// The case, if it exists AND this Focus opened it.
async fn own_case(
    conn: &mut SqliteConnection,
    ctx: &WriterContext<'_>,
    case_id: &str,
) -> Result<case_store::CaseRecord, StoreWriteError> {
    case_store::get_case(conn, case_id)
        .await?
        .filter(|c| c.focus_id == ctx.focus_id)
        .ok_or(StoreWriteError::CaseNotInFocus)
}

/// Argument access over the pass's variable view.
struct Args<'a> {
    op: &'a StoreOp,
    vars: &'a HashMap<String, String>,
}

impl<'a> Args<'a> {
    fn invalid(&self, slot: Slot, reason: &'static str) -> StoreWriteError {
        StoreWriteError::ArgumentInvalid {
            op: self.op.op.as_str(),
            slot: slot.as_str(),
            reason,
        }
    }

    /// The trimmed value of a configured slot. A configured slot whose variable
    /// is unset is an error, an empty value is Some("").
    fn raw(&self, slot: Slot) -> Result<Option<&'a str>, StoreWriteError> {
        let Some(var) = self.op.arg(slot) else {
            return Ok(None);
        };
        match self.vars.get(var) {
            Some(v) => Ok(Some(v.trim())),
            None => Err(StoreWriteError::ArgumentUnset {
                op: self.op.op.as_str(),
                slot: slot.as_str(),
                var: var.to_owned(),
            }),
        }
    }

    /// A required slot: configured (LOAD guarantees it), set and non-empty.
    fn required(&self, slot: Slot) -> Result<&'a str, StoreWriteError> {
        match self.raw(slot)? {
            Some(v) if !v.is_empty() => Ok(v),
            _ => Err(StoreWriteError::ArgumentEmpty {
                op: self.op.op.as_str(),
                slot: slot.as_str(),
            }),
        }
    }

    /// An optional slot: None when not configured or empty, an error when
    /// configured and its variable is unset.
    fn optional(&self, slot: Slot) -> Result<Option<&'a str>, StoreWriteError> {
        Ok(self.raw(slot)?.filter(|v| !v.is_empty()))
    }

    /// A slot whose value is a JSON array of non-empty strings, at most
    /// MAX_LIST_ITEMS of them. None when the slot is not configured or empty.
    fn list(&self, slot: Slot) -> Result<Option<Vec<String>>, StoreWriteError> {
        let Some(text) = self.optional(slot)? else {
            return Ok(None);
        };
        let items: Vec<String> = serde_json::from_str(text)
            .map_err(|_| self.invalid(slot, "not a JSON array of strings"))?;
        if items.len() > MAX_LIST_ITEMS {
            return Err(self.invalid(slot, "too many items"));
        }
        let items: Vec<String> = items.iter().map(|s| s.trim().to_owned()).collect();
        if items.iter().any(String::is_empty) {
            return Err(self.invalid(slot, "an item is empty"));
        }
        if items.is_empty() {
            return Ok(None);
        }
        Ok(Some(items))
    }
}
