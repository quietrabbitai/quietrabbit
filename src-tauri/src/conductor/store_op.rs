// src-tauri/src/conductor/store_op.rs
//
// items.id=887 (decisions.id=939 section 4, decisions.id=938): the TYPED STORE
// WRITER of a composition row, and its LOAD validation.
//
// A focus_block_compositions row may carry store_op, store_target and
// store_args (shared_041.sql). Such a row is a STORE WRITER: it makes no model
// call and records nothing on the TaskTrack. When the run reaches it (and its
// guard, items.id=878, is true) it calls exactly one typed, latch-gated store
// function (conductor::store_writer). The shape mirrors the guard columns:
// closed, typed, database-only, no expression string, no YAML syntax.
//
// THE CLOSED SET OF OPERATIONS (one per store function, items.id=881):
//
//   latch            session_store::latch_write_suppressed (the one-time chat)
//   session_put      session_store::put_session_variable
//   case_open        case_store::open_case
//   case_update      case_store::update_case
//   strategy_append  case_store::append_case_strategy (with rung_at)
//   delta_record     case_store::record_config_delta (with source_case_id)
//   delta_confirm    case_store::set_delta_status -> confirmed-active, for a
//                    configuration change named by its HANDLE (items.id=889)
//   delta_changed    case_store::set_delta_entity_status -> believed-changed
//                    for one delta and entity id (the event hook of
//                    decisions.id=510, by ids; a model cannot reach those ids,
//                    so entity_changed below is the one a Focus uses)
//
// items.id=891 adds two more, the FOCUS-OWNED ENTITY writers (decisions.id=507,
// 940 ruling 2). They write the persona's personal.db, not outputs.db, and are
// run by conductor::entity_writer:
//
//   entity_find_or_create  persistence::owned_entity_store: one device the Focus
//                          owns, found by a handle, by name, or by the replay of
//                          this very row, else created. PUBLISHES the entity id.
//   entity_fact_put        owned_entity_store::put_owned_fact: one catalogued fact
//                          (conductor::device_fields) on an entity this pass
//                          published. An unknown field fails the pass.
//
// items.id=895 (decisions.id=942 point 5) adds two more, the PER-PASS ARTIFACT
// writers. They write pass_artifacts in the persona's outputs.db
// (persistence::pass_artifact_store), the text a later chat pass reads back as a
// bound name (bound_spec.rs, source Artifact):
//
//   artifact_put     pass_artifact_store::put_artifact: store_target is the
//                    artifact's bound name (content_brief, voice_guidance,
//                    validation_report or delta_items), the one slot `value` is
//                    the variable that supplies the text (an earlier step's
//                    output). Gated by the collection consent: it saves text
//                    about what the user is writing.
//   artifact_clear   pass_artifact_store::clear_artifact for that name, no
//                    slots. Not gated, like session_clear: it removes data.
//
// A pass reads an artifact only through the bound name, once, at pass start
// (seeding.rs), so a value written in a pass is read by the NEXT pass. The name
// is the artifact's alone: LOAD refuses it as a row's output_var, as a
// session_put target and as the argument of any writer.
//
// items.id=387 adds one more, the HAND-OFF RETURN writer (decisions.id=942 point 7),
// run by conductor::handoff_writer:
//
//   handoff_ingest   persistence::handoff_draft_store::store_draft plus
//                    session_store::settle_handoff_return: the pasted reply of a
//                    manual hand-off is stored as an untrusted Library draft, and
//                    in one transaction handoff_pending is cleared and
//                    handoff_received set to the draft's H- handle. store_target is
//                    REQUIRED: the pass variable the handle is published under.
//                    NO SLOTS. LOAD requires a guard on handoff_pending with
//                    `present`. Gated by the collection consent (it saves the
//                    user's text).
//
// THE SINGLE EXCEPTION TO "RAW USER TEXT IS NEVER A STORE ARGUMENT". Every writer
// above takes its values from named variables, and LOAD refuses a system token
// (below). handoff_ingest is the ONE operation that reads the raw text of the turn
// the pass is answering, and it does so without a slot: a variable would be a
// model's copy of the paste, and the point is the exact text. The turn travels as
// WriterContext.turn_text (a TurnText borrowed from the run's CurrentTurn), which
// only conductor::handoff_writer can read; it is never persisted, serialized into a
// checkpoint or snapshot, or logged, and its Debug redacts it. A source scan
// (handoff_writer_pins in store_op_tests.rs) pins that no other file reads it.
//
// A model never names an entity id. Every entity slot of case_open, case_link,
// delta_record, delta_changed and entity_fact_put must be the id PUBLISHED by an
// earlier entity_find_or_create row (LOAD), the model picks an existing device by
// an opaque E- handle (a Handle slot), and the store checks again that the entity
// is this Focus's and active before the write. Both new operations save the
// user's equipment, so both are gated by the collection consent. They are refused
// at LOAD in a Focus that is not focus_level, because the consent gate never
// consults such a Focus.
//
// items.id=889 adds four more (decisions.id=940, handoff 629 gap G2):
//
//   case_select      session_store::put_session_variable on active_case_id,
//                    from a ticket HANDLE resolved store-side to an open case
//                    this Focus opened (persistence::handles)
//   case_link        case_store::link_case_entity (an affected entity)
//   entity_changed   case_store::mark_entity_deltas_changed: an event on the
//                    active case's primary entity moves every delta of that
//                    entity this Focus recorded to believed-changed
//   session_clear    session_store::clear_session_state: resets locked_intent
//                    and active_case_id, never the latch
//
// A HANDLE slot (case_select, delta_confirm) takes the OUTPUT of a model step:
// the model is shown handles in derived text and names one. It is never an id,
// so LOAD refuses a bound name or a published id there.
//
// COLUMN SHAPE:
//   store_target  session_put: the session variable written (a bound name with
//                 source Session, never write_mode). case_open and delta_record:
//                 optional, the PASS VARIABLE the new case id or delta id is
//                 published under, for later rows' arguments and guards. Every
//                 other operation: NULL.
//   store_args    a JSON object {slot: variable_name}. Each slot of the
//                 operation (the table below) names the VARIABLE that supplies
//                 it. There are no literals and no expressions: a value always
//                 comes from a named variable, so LOAD can prove where it came
//                 from. An unknown slot, a missing required slot and a duplicate
//                 slot each fail LOAD.
//
// WHAT AN ARGUMENT VARIABLE MAY BE (LOAD, check_composition):
//   * the output variable of an EARLIER row, or the pass variable an earlier
//     store writer publishes (a write row cannot sit before the data it needs;
//     a name only a LATER row produces has its own error, and guard.rs checks
//     per path that the producing row actually runs before the writer), or
//   * a session-bound name (bound_spec.rs, source Session: locked_intent,
//     active_case_id), seeded at pass start. An OPTIONAL one (it can be absent)
//     needs a guard on the same row that names it with a value or present test,
//     so the row only runs when it is set.
//   NOT allowed: write_mode (only the latch operation touches the latch), a
//   derived bound name (case_context and the like are read-only renderings, not
//   data to write back), a system token (raw user text and persona context are
//   never an argument) and an unknown name.
//
// TAINT (conductor::taint, decisions.id=939 section 3). A writer's template is
// empty, so the closure neither marks it nor needs to: it records nothing on the
// track, so it adds nothing to extraction or to {previous_output}. Its
// arguments MAY be tainted variables. That is the use of the strategy log (a
// summary written by a step whose prompt held case_context is tainted), and the
// value goes back into the same persona's store, scoped to the running Focus
// (store_writer). The extraction exclusion of the producing step is unchanged,
// and nothing here lets a tainted value reach an external prompt: that is the
// declared-inputs lint (declared_inputs.rs: STORE_NAMES_EXTERNAL_ALLOWED, true since
// items.id=884, and the closed device list) and the outbound review. A published id is derived
// from the run, not from content, and is not tainted.

use std::collections::HashSet;

use serde::de::{MapAccess, Visitor};
use serde::{Deserialize, Deserializer};

use crate::conductor::bound_spec::{
    self, Presence, Source, HANDOFF_NAMES, HANDOFF_PENDING, LAUNCH_NAMES, WRITE_MODE,
};
use crate::conductor::guard::Condition;
use crate::conductor::tokens::{is_system_token, ActionDefinition};
use crate::persistence::focus_composition_store::CollectionPosture;

// ---------------------------------------------------------------------------
// Operations and slots
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Op {
    Latch,
    SessionPut,
    CaseOpen,
    CaseUpdate,
    StrategyAppend,
    DeltaRecord,
    DeltaConfirm,
    DeltaChanged,
    CaseSelect,
    CaseLink,
    EntityChanged,
    SessionClear,
    EntityFindOrCreate,
    EntityFactPut,
    ArtifactPut,
    ArtifactClear,
    HandoffIngest,
    /// items.id=903: validate a StructuredBrief JSON, render it, store content_brief.
    BriefPut,
    /// items.id=903: append one headed section to working_draft.
    ArtifactAppend,
}

impl Op {
    /// The closed set, in documentation order.
    pub const ALL: [Op; 19] = [
        Op::Latch,
        Op::SessionPut,
        Op::CaseOpen,
        Op::CaseUpdate,
        Op::StrategyAppend,
        Op::DeltaRecord,
        Op::DeltaConfirm,
        Op::DeltaChanged,
        Op::CaseSelect,
        Op::CaseLink,
        Op::EntityChanged,
        Op::SessionClear,
        Op::EntityFindOrCreate,
        Op::EntityFactPut,
        Op::ArtifactPut,
        Op::ArtifactClear,
        Op::HandoffIngest,
        Op::BriefPut,
        Op::ArtifactAppend,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Op::Latch => "latch",
            Op::SessionPut => "session_put",
            Op::CaseOpen => "case_open",
            Op::CaseUpdate => "case_update",
            Op::StrategyAppend => "strategy_append",
            Op::DeltaRecord => "delta_record",
            Op::DeltaConfirm => "delta_confirm",
            Op::DeltaChanged => "delta_changed",
            Op::CaseSelect => "case_select",
            Op::CaseLink => "case_link",
            Op::EntityChanged => "entity_changed",
            Op::SessionClear => "session_clear",
            Op::EntityFindOrCreate => "entity_find_or_create",
            Op::EntityFactPut => "entity_fact_put",
            Op::ArtifactPut => "artifact_put",
            Op::ArtifactClear => "artifact_clear",
            Op::HandoffIngest => "handoff_ingest",
            Op::BriefPut => "brief_put",
            Op::ArtifactAppend => "artifact_append",
        }
    }

    /// items.id=891: true for the two operations that write a Focus-owned entity
    /// in personal.db (conductor::entity_writer), as opposed to the stores in
    /// outputs.db. Exhaustive on purpose.
    pub fn is_entity_op(self) -> bool {
        match self {
            Op::EntityFindOrCreate | Op::EntityFactPut => true,
            Op::Latch
            | Op::SessionPut
            | Op::CaseOpen
            | Op::CaseUpdate
            | Op::StrategyAppend
            | Op::DeltaRecord
            | Op::DeltaConfirm
            | Op::DeltaChanged
            | Op::CaseSelect
            | Op::CaseLink
            | Op::EntityChanged
            | Op::SessionClear
            | Op::ArtifactPut
            | Op::ArtifactClear
            | Op::HandoffIngest
            | Op::BriefPut
            | Op::ArtifactAppend => false,
        }
    }

    /// items.id=891: true for the operations that take an ENTITY ID argument, which
    /// the lifecycle verifies (owned by this Focus, active) before the write:
    /// case_open, case_link and delta_changed (slot entity), delta_record (slot
    /// entities) and entity_fact_put (slot entity). Exhaustive on purpose.
    pub fn takes_entity_ids(self) -> bool {
        match self {
            Op::CaseOpen
            | Op::CaseLink
            | Op::DeltaRecord
            | Op::DeltaChanged
            | Op::EntityFactPut => true,
            Op::Latch
            | Op::SessionPut
            | Op::CaseUpdate
            | Op::StrategyAppend
            | Op::DeltaConfirm
            | Op::CaseSelect
            | Op::EntityChanged
            | Op::SessionClear
            | Op::EntityFindOrCreate
            | Op::ArtifactPut
            | Op::ArtifactClear
            | Op::HandoffIngest
            | Op::BriefPut
            | Op::ArtifactAppend => false,
        }
    }

    /// True for the operations that write the CASE itself (its record, its
    /// strategy log or its entity links). A terminal case takes none of them, so
    /// the close-ticket lint (guard.rs) keeps every one of them before a closing
    /// case_update on any path.
    pub fn writes_case(self) -> bool {
        matches!(self, Op::CaseUpdate | Op::StrategyAppend | Op::CaseLink)
    }

    /// items.id=920 (decisions.id=944): true for the operations that SAVE
    /// something about the user's situation, which a focus_level Focus may do
    /// only after the one setup consent (store_writer::Collection). Exhaustive on
    /// purpose, so a new operation must be classified here.
    ///   NOT gated: Latch (it SUPPRESSES writes, so refusing it would only widen
    ///   what a no-consent chat can save) and SessionClear (it REMOVES data, which
    ///   the user must always be able to cause).
    ///
    /// items.id=895: artifact_put saves text (a brief, voice guidance, a report)
    /// and is gated. artifact_clear removes data, like session_clear, and is not.
    ///
    /// items.id=387: handoff_ingest saves the user's pasted text and is gated.
    pub fn requires_collection_consent(self) -> bool {
        match self {
            Op::Latch | Op::SessionClear | Op::ArtifactClear => false,
            Op::SessionPut
            | Op::CaseOpen
            | Op::CaseUpdate
            | Op::StrategyAppend
            | Op::DeltaRecord
            | Op::DeltaConfirm
            | Op::DeltaChanged
            | Op::CaseSelect
            | Op::CaseLink
            | Op::EntityChanged
            | Op::EntityFindOrCreate
            | Op::EntityFactPut
            | Op::ArtifactPut
            | Op::HandoffIngest
            | Op::BriefPut
            | Op::ArtifactAppend => true,
        }
    }

    pub fn parse(s: &str) -> Option<Op> {
        Op::ALL.iter().copied().find(|o| o.as_str() == s)
    }

    fn spec(self) -> &'static OpSpec {
        OP_SPECS
            .iter()
            .find(|s| s.op == self)
            .expect("every operation has a spec")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Slot {
    /// session_put: the value.
    Value,
    /// case_open: the primary subject entity id. case_link: the entity to link.
    /// delta_changed: the entity of the delta.
    Entity,
    /// case_open: the subject label.
    Label,
    /// case_open and case_update: the symptoms text.
    Symptoms,
    /// case_update, strategy_append, case_link, entity_changed and delta_record
    /// (optional): a case id.
    Case,
    /// case_update: the lifecycle state.
    State,
    /// case_update: the escalation rung, 1 to 5.
    Rung,
    /// case_update: the resolution path.
    ResolutionPath,
    /// case_update and strategy_append: the outcome text.
    Outcome,
    /// case_update: durable facts, JSON.
    DurableFacts,
    /// strategy_append: the strategy text.
    Strategy,
    /// strategy_append: the user signal, optional.
    UserSignal,
    /// delta_record: a JSON array of entity ids.
    Entities,
    /// delta_record: the change description.
    Description,
    /// delta_record: a JSON array of relevance tags, optional.
    Tags,
    /// delta_changed: the delta id.
    Delta,
    /// case_select: a ticket handle. delta_confirm: a configuration-change
    /// handle. entity_find_or_create (optional): a device handle. The value of a
    /// model step's output, resolved store-side.
    Handle,
    /// entity_find_or_create (optional): the subject class, colon-separated.
    Class,
    /// entity_fact_put: the catalogued field name.
    Field,
}

impl Slot {
    pub fn as_str(self) -> &'static str {
        match self {
            Slot::Value => "value",
            Slot::Entity => "entity",
            Slot::Label => "label",
            Slot::Symptoms => "symptoms",
            Slot::Case => "case",
            Slot::State => "state",
            Slot::Rung => "rung",
            Slot::ResolutionPath => "resolution_path",
            Slot::Outcome => "outcome",
            Slot::DurableFacts => "durable_facts",
            Slot::Strategy => "strategy",
            Slot::UserSignal => "user_signal",
            Slot::Entities => "entities",
            Slot::Description => "description",
            Slot::Tags => "tags",
            Slot::Delta => "delta",
            Slot::Handle => "handle",
            Slot::Class => "class",
            Slot::Field => "field",
        }
    }
}

/// What store_target means for an operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetRule {
    /// Must be NULL.
    None,
    /// Required: a session variable name.
    SessionName,
    /// Optional: the pass variable a new id is published under.
    PublishId,
    /// Required: the pass variable the entity id is published under, because every
    /// later entity slot must name it (items.id=891).
    PublishIdRequired,
    /// Required: an artifact bound name (bound_spec source Artifact), items.id=895.
    ArtifactName,
}

pub struct OpSpec {
    pub op: Op,
    pub target: TargetRule,
    pub required: &'static [Slot],
    pub optional: &'static [Slot],
    /// A row of this operation must configure at least one optional slot.
    pub needs_an_optional: bool,
}

/// The slot table, pinned by a test: a change here is a design change.
pub const OP_SPECS: &[OpSpec] = &[
    OpSpec {
        op: Op::Latch,
        target: TargetRule::None,
        required: &[],
        optional: &[],
        needs_an_optional: false,
    },
    OpSpec {
        op: Op::SessionPut,
        target: TargetRule::SessionName,
        required: &[Slot::Value],
        optional: &[],
        needs_an_optional: false,
    },
    OpSpec {
        op: Op::CaseOpen,
        target: TargetRule::PublishId,
        required: &[Slot::Entity, Slot::Label],
        optional: &[Slot::Symptoms],
        needs_an_optional: false,
    },
    OpSpec {
        op: Op::CaseUpdate,
        target: TargetRule::None,
        required: &[Slot::Case],
        optional: &[
            Slot::State,
            Slot::Rung,
            Slot::ResolutionPath,
            Slot::Symptoms,
            Slot::Outcome,
            Slot::DurableFacts,
        ],
        needs_an_optional: true,
    },
    OpSpec {
        op: Op::StrategyAppend,
        target: TargetRule::None,
        required: &[Slot::Case, Slot::Strategy, Slot::Outcome],
        optional: &[Slot::UserSignal],
        needs_an_optional: false,
    },
    OpSpec {
        op: Op::DeltaRecord,
        target: TargetRule::PublishId,
        required: &[Slot::Entities, Slot::Description],
        optional: &[Slot::Tags, Slot::Case],
        needs_an_optional: false,
    },
    OpSpec {
        op: Op::DeltaConfirm,
        target: TargetRule::None,
        required: &[Slot::Handle],
        optional: &[],
        needs_an_optional: false,
    },
    OpSpec {
        op: Op::DeltaChanged,
        target: TargetRule::None,
        required: &[Slot::Delta, Slot::Entity],
        optional: &[],
        needs_an_optional: false,
    },
    OpSpec {
        op: Op::CaseSelect,
        target: TargetRule::None,
        required: &[Slot::Handle],
        optional: &[],
        needs_an_optional: false,
    },
    OpSpec {
        op: Op::CaseLink,
        target: TargetRule::None,
        required: &[Slot::Case, Slot::Entity],
        optional: &[],
        needs_an_optional: false,
    },
    OpSpec {
        op: Op::EntityChanged,
        target: TargetRule::None,
        required: &[Slot::Case],
        optional: &[],
        needs_an_optional: false,
    },
    OpSpec {
        op: Op::SessionClear,
        target: TargetRule::None,
        required: &[],
        optional: &[],
        needs_an_optional: false,
    },
    OpSpec {
        op: Op::EntityFindOrCreate,
        target: TargetRule::PublishIdRequired,
        required: &[Slot::Label],
        optional: &[Slot::Handle, Slot::Class],
        needs_an_optional: false,
    },
    OpSpec {
        op: Op::EntityFactPut,
        target: TargetRule::None,
        required: &[Slot::Entity, Slot::Field, Slot::Value],
        optional: &[],
        needs_an_optional: false,
    },
    OpSpec {
        op: Op::ArtifactPut,
        target: TargetRule::ArtifactName,
        required: &[Slot::Value],
        optional: &[],
        needs_an_optional: false,
    },
    OpSpec {
        op: Op::ArtifactClear,
        target: TargetRule::ArtifactName,
        required: &[],
        optional: &[],
        needs_an_optional: false,
    },
    // items.id=387: no slots (the pasted turn is not a slot, see the header) and a
    // required target for the published handle.
    OpSpec {
        op: Op::HandoffIngest,
        target: TargetRule::PublishIdRequired,
        required: &[],
        optional: &[],
        needs_an_optional: false,
    },
    // items.id=903: the target is the pass variable the OUTCOME word is published
    // under (brief_put: stored or invalid; artifact_append: appended or too_long), so
    // a later reply row is guarded on what the store did, never on model text. The
    // artifact each one writes is fixed (content_brief, working_draft).
    OpSpec {
        op: Op::BriefPut,
        target: TargetRule::PublishIdRequired,
        required: &[Slot::Value],
        optional: &[],
        needs_an_optional: false,
    },
    OpSpec {
        op: Op::ArtifactAppend,
        target: TargetRule::PublishIdRequired,
        required: &[Slot::Value],
        optional: &[],
        needs_an_optional: false,
    },
];

/// Longest variable name a row may name.
const VAR_NAME_MAX: usize = 64;

// ---------------------------------------------------------------------------
// The typed row
// ---------------------------------------------------------------------------

/// A parsed store writer. An invalid operation/slot combination cannot be built.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreOp {
    pub op: Op,
    pub target: Option<String>,
    /// (slot, variable name), in the operation's table order.
    pub args: Vec<(Slot, String)>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StoreOpError {
    #[error("store_target or store_args is set without store_op (set: {0}): the three columns go together")]
    HalfSet(String),
    #[error("unknown store_op '{0}' (expected one of latch, session_put, case_open, case_update, strategy_append, delta_record, delta_confirm, delta_changed, case_select, case_link, entity_changed, session_clear, entity_find_or_create, entity_fact_put, artifact_put, artifact_clear, handoff_ingest)")]
    UnknownOp(String),
    #[error("store_op '{0}' needs a store_target")]
    TargetRequired(&'static str),
    #[error("store_op '{0}' takes no store_target")]
    TargetNotAllowed(&'static str),
    #[error(
        "store_target '{0}' is not a usable name (letters, digits and underscores, at most 64)"
    )]
    BadTarget(String),
    #[error("store_op '{0}' takes no store_args")]
    ArgsNotAllowed(&'static str),
    #[error("store_op '{0}' needs store_args")]
    ArgsRequired(&'static str),
    #[error("store_args is not a JSON object of slot names to variable names: {0}")]
    BadArgs(String),
    #[error("store_args names slot '{slot}' twice")]
    DuplicateSlot { slot: String },
    #[error("store_op '{op}' has no slot '{slot}'")]
    UnknownSlot { op: &'static str, slot: String },
    #[error("store_op '{op}' needs slot '{slot}'")]
    MissingSlot {
        op: &'static str,
        slot: &'static str,
    },
    #[error("store_op '{0}' must set at least one field to change")]
    NothingToChange(&'static str),
    #[error("slot '{slot}' names '{var}', which is not a usable variable name (letters, digits and underscores, at most 64)")]
    BadVariable { slot: &'static str, var: String },

    // -- composition checks (check_composition) ----------------------------
    #[error("step '{action_id}': session_put target '{name}' is not a registered session variable (the session names are those in bound_spec with source session)")]
    UnknownSessionName { action_id: String, name: String },
    #[error("step '{action_id}': session_put target '{name}' is a derived, read-only name, not a session variable")]
    DerivedSessionName { action_id: String, name: String },
    #[error("step '{action_id}': '{WRITE_MODE}' can only be set by the latch operation, not by session_put")]
    WriteModeNotPutable { action_id: String },
    #[error("step '{action_id}': published variable '{var}' collides with a bound name, a system token or another row's variable")]
    PublishedNameCollision { action_id: String, var: String },
    #[error("step '{action_id}': slot '{slot}' names '{var}', which is produced only by a LATER step: a write row cannot sit before the data it needs")]
    ArgumentFromLaterRow {
        action_id: String,
        slot: &'static str,
        var: String,
    },
    #[error("step '{action_id}': slot '{slot}' names '{var}', which no earlier step produces and which is not a session variable")]
    ArgumentUnknown {
        action_id: String,
        slot: &'static str,
        var: String,
    },
    #[error("step '{action_id}': slot '{slot}' names '{var}', a derived name that is a read-only rendering of the store and cannot be written back")]
    ArgumentDerived {
        action_id: String,
        slot: &'static str,
        var: String,
    },
    #[error("step '{action_id}': slot '{slot}' names '{var}', which a store writer cannot read as an argument ({reason})")]
    ArgumentNotAllowed {
        action_id: String,
        slot: &'static str,
        var: String,
        reason: &'static str,
    },
    #[error("step '{action_id}': slot '{slot}' names '{var}', which is {reason}: a handle is the output of a model step that was shown handles in derived text, never an id and never a seeded name")]
    HandleNotFromModel {
        action_id: String,
        slot: &'static str,
        var: String,
        reason: &'static str,
    },
    #[error("step '{action_id}': slot '{slot}' names '{var}', a session variable that can be absent: the row needs a guard on '{var}' (eq, ne, in or present) so it only runs when the value is set")]
    OptionalArgumentUnguarded {
        action_id: String,
        slot: &'static str,
        var: String,
    },
    // -- items.id=891: the Focus-owned entity writers -------------------------
    #[error("step '{action_id}': slot '{slot}' names '{var}', which is not the id published by an EARLIER entity_find_or_create row: a model never names an entity id, so an entity slot must name the variable an earlier entity_find_or_create row publishes")]
    EntitySlotNotPublished {
        action_id: String,
        slot: &'static str,
        var: String,
    },
    // -- items.id=895: the per-pass artifact writers ---------------------------
    #[error("step '{action_id}': {op} target '{name}' is not an artifact name (the artifact names are content_brief, voice_guidance, validation_report and delta_items)")]
    UnknownArtifactName {
        action_id: String,
        op: &'static str,
        name: String,
    },
    #[error("step '{action_id}': session_put target '{name}' is a per-pass artifact, written only by artifact_put")]
    ArtifactNotSessionName { action_id: String, name: String },
    #[error("step '{action_id}': slot '{slot}' names '{var}', a per-pass artifact that a template reads as a bound name and a store writer never takes as an argument")]
    ArgumentArtifact {
        action_id: String,
        slot: &'static str,
        var: String,
    },
    // -- items.id=387: the hand-off names and the return writer ------------------
    #[error("step '{action_id}': handoff_ingest needs a guard on '{HANDOFF_PENDING}' with the condition present, so the row only runs while a hand-off is waiting for its reply")]
    HandoffIngestUnguarded { action_id: String },
    #[error("step '{action_id}': session_put target '{name}' is a hand-off name, written only by the hand-off operations")]
    HandoffNameNotSessionPutable { action_id: String, name: String },
    #[error("step '{action_id}': slot '{slot}' names '{var}', a hand-off name that only the hand-off operations write and that is never a writer's argument")]
    ArgumentHandoffName {
        action_id: String,
        slot: &'static str,
        var: String,
    },
    #[error("step '{action_id}': session_put target '{name}' is a launch name, answered by the user before the run and read once when the run starts, never written by a Focus row")]
    LaunchNameNotSessionPutable { action_id: String, name: String },
    #[error("step '{action_id}': slot '{slot}' names '{var}', a launch name that is read when the run starts and is never a writer's argument")]
    ArgumentLaunchName {
        action_id: String,
        slot: &'static str,
        var: String,
    },
    #[error("step '{action_id}': store_op '{op}' saves facts about the user's equipment and is allowed only in a Focus whose collection_posture is focus_level: the consent gate never consults a per_field Focus, so the row would write without the one setup consent")]
    EntityOpNeedsFocusLevel { action_id: String, op: &'static str },
}

/// True for the slots that carry an entity id: the entity of case_open,
/// case_link, delta_changed and entity_fact_put, and the entities of
/// delta_record.
fn is_entity_id_slot(op: Op, slot: Slot) -> bool {
    matches!(
        (op, slot),
        (
            Op::CaseOpen | Op::CaseLink | Op::DeltaChanged | Op::EntityFactPut,
            Slot::Entity
        ) | (Op::DeltaRecord, Slot::Entities)
    )
}

/// Names a row may use for variables and published ids.
fn valid_var_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= VAR_NAME_MAX
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// A JSON object read as ordered (key, string) pairs, so a duplicate key is
/// seen. serde_json's own map would keep the last one silently.
struct Pairs(Vec<(String, String)>);

impl<'de> Deserialize<'de> for Pairs {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Pairs;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("an object whose values are all strings")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Pairs, A::Error> {
                let mut out = Vec::new();
                while let Some((k, v)) = map.next_entry::<String, String>()? {
                    out.push((k, v));
                }
                Ok(Pairs(out))
            }
        }
        d.deserialize_map(V)
    }
}

impl StoreOp {
    /// Builds a store writer from the three nullable columns. `Ok(None)` for a
    /// row that is not a writer (all three NULL). Every other malformed
    /// combination is its own error, so LOAD can say which part is wrong.
    pub fn from_columns(
        op: Option<&str>,
        target: Option<&str>,
        args: Option<&str>,
    ) -> Result<Option<StoreOp>, StoreOpError> {
        let Some(op_text) = op else {
            let set: Vec<&str> = [
                (target.is_some(), "store_target"),
                (args.is_some(), "store_args"),
            ]
            .iter()
            .filter(|(s, _)| *s)
            .map(|(_, n)| *n)
            .collect();
            return if set.is_empty() {
                Ok(None)
            } else {
                Err(StoreOpError::HalfSet(set.join(", ")))
            };
        };
        let op = Op::parse(op_text.trim())
            .ok_or_else(|| StoreOpError::UnknownOp(op_text.trim().to_owned()))?;
        let spec = op.spec();
        let name = op.as_str();

        let target = match (spec.target, target) {
            (TargetRule::None, Some(_)) => return Err(StoreOpError::TargetNotAllowed(name)),
            (TargetRule::None, None) | (TargetRule::PublishId, None) => None,
            (TargetRule::SessionName, None)
            | (TargetRule::PublishIdRequired, None)
            | (TargetRule::ArtifactName, None) => return Err(StoreOpError::TargetRequired(name)),
            (TargetRule::SessionName, Some(t))
            | (TargetRule::PublishId, Some(t))
            | (TargetRule::PublishIdRequired, Some(t))
            | (TargetRule::ArtifactName, Some(t)) => {
                let t = t.trim();
                if !valid_var_name(t) {
                    return Err(StoreOpError::BadTarget(t.to_owned()));
                }
                Some(t.to_owned())
            }
        };

        let no_slots = spec.required.is_empty() && spec.optional.is_empty();
        let pairs = match (no_slots, args) {
            (true, Some(_)) => return Err(StoreOpError::ArgsNotAllowed(name)),
            (true, None) => Vec::new(),
            (false, None) => return Err(StoreOpError::ArgsRequired(name)),
            (false, Some(text)) => {
                let Pairs(pairs) = serde_json::from_str::<Pairs>(text)
                    .map_err(|e| StoreOpError::BadArgs(e.to_string()))?;
                pairs
            }
        };

        // Every key is a slot of this operation, and none repeats.
        let mut seen: HashSet<&str> = HashSet::new();
        for (k, _) in &pairs {
            if !seen.insert(k.as_str()) {
                return Err(StoreOpError::DuplicateSlot { slot: k.clone() });
            }
            let known = spec
                .required
                .iter()
                .chain(spec.optional.iter())
                .any(|s| s.as_str() == k);
            if !known {
                return Err(StoreOpError::UnknownSlot {
                    op: name,
                    slot: k.clone(),
                });
            }
        }
        // Canonical order: the table's. Required slots must all be present.
        let mut args = Vec::new();
        for slot in spec.required.iter().chain(spec.optional.iter()) {
            let found = pairs.iter().find(|(k, _)| k == slot.as_str());
            match found {
                Some((_, var)) => {
                    let var = var.trim();
                    if !valid_var_name(var) {
                        return Err(StoreOpError::BadVariable {
                            slot: slot.as_str(),
                            var: var.to_owned(),
                        });
                    }
                    args.push((*slot, var.to_owned()));
                }
                None if spec.required.contains(slot) => {
                    return Err(StoreOpError::MissingSlot {
                        op: name,
                        slot: slot.as_str(),
                    });
                }
                None => {}
            }
        }
        if spec.needs_an_optional && !args.iter().any(|(s, _)| spec.optional.contains(s)) {
            return Err(StoreOpError::NothingToChange(name));
        }
        Ok(Some(StoreOp { op, target, args }))
    }

    /// The variable this row supplies for `slot`, if configured.
    pub fn arg(&self, slot: Slot) -> Option<&str> {
        self.args
            .iter()
            .find(|(s, _)| *s == slot)
            .map(|(_, v)| v.as_str())
    }

    /// The pass variable this row publishes a new id under (case_open and
    /// delta_record with a store_target, entity_find_or_create and handoff_ingest
    /// always), else None.
    pub fn published_var(&self) -> Option<&str> {
        match self.op {
            Op::CaseOpen
            | Op::DeltaRecord
            | Op::EntityFindOrCreate
            | Op::HandoffIngest
            | Op::BriefPut
            | Op::ArtifactAppend => self.target.as_deref(),
            _ => None,
        }
    }

    /// For a case_update that sets the lifecycle state, the variable that
    /// supplies it (the close-ticket lint, guard.rs). None for every other row.
    pub fn state_var(&self) -> Option<&str> {
        match self.op {
            Op::CaseUpdate => self.arg(Slot::State),
            _ => None,
        }
    }

    /// Every variable name this row reads.
    pub fn arg_vars(&self) -> impl Iterator<Item = &str> {
        self.args.iter().map(|(_, v)| v.as_str())
    }
}

// ---------------------------------------------------------------------------
// LOAD validation over a whole composition
// ---------------------------------------------------------------------------

/// The variable a row records or publishes when it runs: an ordinary step's
/// output variable, a block's declared one, or a store writer's published id.
/// Computed by lifecycle::row_output_vars and passed in as `row_vars`.
///
/// Checks every store writer in `steps` against the registered names and the
/// rows around it. Per-row shape (parse, slot table) is StoreOp::from_columns,
/// run by the loader; this is the cross-row part. The per-PATH part ("the row
/// that produces an argument actually runs before the writer on every path
/// that runs the writer") is guard::analyze_composition.
///
/// items.id=891: `posture` is the Focus's declared collection_posture. The entity
/// writers are refused in a per_field Focus (the consent gate never consults it),
/// and every entity-id slot must name a variable an earlier entity_find_or_create
/// row publishes.
pub fn check_composition(
    steps: &[ActionDefinition],
    row_vars: &[Option<String>],
    posture: CollectionPosture,
) -> Result<(), StoreOpError> {
    debug_assert_eq!(steps.len(), row_vars.len());
    for (i, step) in steps.iter().enumerate() {
        let Some(op) = &step.store_op else { continue };
        let action_id = step.action_id.clone();

        if op.op.is_entity_op() && posture != CollectionPosture::FocusLevel {
            return Err(StoreOpError::EntityOpNeedsFocusLevel {
                action_id,
                op: op.op.as_str(),
            });
        }

        // items.id=895: the target of an artifact writer is one of the artifact
        // names, and nothing else.
        if matches!(op.op, Op::ArtifactPut | Op::ArtifactClear) {
            let name = op.target.as_deref().unwrap_or("");
            if !bound_spec::spec(name).is_some_and(|s| s.source == Source::Artifact) {
                return Err(StoreOpError::UnknownArtifactName {
                    action_id,
                    op: op.op.as_str(),
                    name: name.to_owned(),
                });
            }
        }

        // items.id=387: handoff_ingest only runs while a hand-off is waiting.
        if op.op == Op::HandoffIngest {
            let guarded = step
                .guards()
                .iter()
                .any(|g| g.var == HANDOFF_PENDING && g.condition == Condition::Present);
            if !guarded {
                return Err(StoreOpError::HandoffIngestUnguarded { action_id });
            }
        }

        // The target of session_put is a registered session name, and the latch
        // is out of its reach.
        if op.op == Op::SessionPut {
            let name = op.target.as_deref().unwrap_or("");
            if HANDOFF_NAMES.contains(&name) {
                return Err(StoreOpError::HandoffNameNotSessionPutable {
                    action_id,
                    name: name.to_owned(),
                });
            }
            if LAUNCH_NAMES.contains(&name) {
                return Err(StoreOpError::LaunchNameNotSessionPutable {
                    action_id,
                    name: name.to_owned(),
                });
            }
            if name == WRITE_MODE {
                return Err(StoreOpError::WriteModeNotPutable { action_id });
            }
            match bound_spec::spec(name) {
                None => {
                    return Err(StoreOpError::UnknownSessionName {
                        action_id,
                        name: name.to_owned(),
                    })
                }
                Some(s) if s.source == Source::Artifact => {
                    return Err(StoreOpError::ArtifactNotSessionName {
                        action_id,
                        name: name.to_owned(),
                    })
                }
                Some(s) if s.source != Source::Session => {
                    return Err(StoreOpError::DerivedSessionName {
                        action_id,
                        name: name.to_owned(),
                    })
                }
                Some(_) => {}
            }
        }

        // A published id is its own name: not a bound name, not a system token,
        // not any other row's variable.
        if let Some(var) = op.published_var() {
            let others = row_vars
                .iter()
                .enumerate()
                .filter(|(j, v)| *j != i && v.as_deref() == Some(var))
                .count();
            if bound_spec::spec(var).is_some() || is_system_token(var) || others > 0 {
                return Err(StoreOpError::PublishedNameCollision {
                    action_id,
                    var: var.to_owned(),
                });
            }
        }

        // Every argument variable.
        for (slot_kind, var) in &op.args {
            let slot = slot_kind.as_str();
            let var = var.as_str();
            // items.id=891: an entity-id slot names the id an earlier
            // entity_find_or_create row published, and nothing else (not a model
            // output, not a seeded name, not another writer's id).
            if is_entity_id_slot(op.op, *slot_kind) {
                let published_earlier = steps[..i].iter().any(|o| {
                    o.store_op.as_ref().is_some_and(|w| {
                        w.op == Op::EntityFindOrCreate && w.published_var() == Some(var)
                    })
                });
                if !published_earlier {
                    return Err(StoreOpError::EntitySlotNotPublished {
                        action_id,
                        slot,
                        var: var.to_owned(),
                    });
                }
                continue;
            }
            // A handle is model output: not a seeded name (active_case_id holds
            // an internal id, locked_intent an intent) and not an id this pass
            // published (an internal id too). Refused before anything else so
            // the reason names the handle rule.
            if *slot_kind == Slot::Handle {
                let bound = bound_spec::spec(var).is_some();
                let published = steps
                    .iter()
                    .any(|o| o.store_op.as_ref().and_then(StoreOp::published_var) == Some(var));
                if bound || published {
                    return Err(StoreOpError::HandleNotFromModel {
                        action_id,
                        slot,
                        var: var.to_owned(),
                        reason: if bound {
                            "a bound name"
                        } else {
                            "an id published by a store writer"
                        },
                    });
                }
            }
            if var == WRITE_MODE {
                return Err(StoreOpError::ArgumentNotAllowed {
                    action_id,
                    slot,
                    var: var.to_owned(),
                    reason: "only the latch operation touches the one-time latch",
                });
            }
            if HANDOFF_NAMES.contains(&var) {
                return Err(StoreOpError::ArgumentHandoffName {
                    action_id,
                    slot,
                    var: var.to_owned(),
                });
            }
            if LAUNCH_NAMES.contains(&var) {
                return Err(StoreOpError::ArgumentLaunchName {
                    action_id,
                    slot,
                    var: var.to_owned(),
                });
            }
            if let Some(s) = bound_spec::spec(var) {
                if s.source == Source::Artifact {
                    return Err(StoreOpError::ArgumentArtifact {
                        action_id,
                        slot,
                        var: var.to_owned(),
                    });
                }
                if s.source == Source::Derived {
                    return Err(StoreOpError::ArgumentDerived {
                        action_id,
                        slot,
                        var: var.to_owned(),
                    });
                }
                if s.presence == Presence::Optional {
                    // items.id=889: ANY clause of the row's AND-guard set that
                    // tests the name for a value or presence covers it. The
                    // clauses are ANDed, so the row cannot run unless that one
                    // holds, whatever else it also requires. (Before the clause
                    // list the single guard had to be it.)
                    let covered = step
                        .guards()
                        .iter()
                        .any(|g| g.var == var && !matches!(g.condition, Condition::Absent));
                    if !covered {
                        return Err(StoreOpError::OptionalArgumentUnguarded {
                            action_id,
                            slot,
                            var: var.to_owned(),
                        });
                    }
                }
                continue;
            }
            if is_system_token(var) {
                return Err(StoreOpError::ArgumentNotAllowed {
                    action_id,
                    slot,
                    var: var.to_owned(),
                    reason: "raw user input and persona context are never a store argument",
                });
            }
            let earlier = row_vars[..i].iter().any(|v| v.as_deref() == Some(var));
            if earlier {
                continue;
            }
            let later = row_vars[i..].iter().any(|v| v.as_deref() == Some(var));
            return Err(if later {
                StoreOpError::ArgumentFromLaterRow {
                    action_id,
                    slot,
                    var: var.to_owned(),
                }
            } else {
                StoreOpError::ArgumentUnknown {
                    action_id,
                    slot,
                    var: var.to_owned(),
                }
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conductor::bound_spec::HANDOFF_RECEIVED;
    use crate::conductor::guard::Guard;
    use crate::conductor::tokens::{ExternalAccess, StepType};
    use std::collections::HashMap;

    fn parse(op: &str, target: Option<&str>, args: Option<&str>) -> Result<StoreOp, StoreOpError> {
        StoreOp::from_columns(Some(op), target, args).map(|o| o.expect("a writer"))
    }

    // -- parsing ------------------------------------------------------------

    #[test]
    fn all_null_is_not_a_writer() {
        assert_eq!(StoreOp::from_columns(None, None, None), Ok(None));
    }

    #[test]
    fn target_or_args_without_an_op_is_half_set() {
        assert!(matches!(
            StoreOp::from_columns(None, Some("x"), None),
            Err(StoreOpError::HalfSet(_))
        ));
        assert!(matches!(
            StoreOp::from_columns(None, None, Some("{}")),
            Err(StoreOpError::HalfSet(_))
        ));
    }

    #[test]
    fn every_operation_in_the_closed_set_parses_with_its_minimum() {
        assert_eq!(parse("latch", None, None).unwrap().op, Op::Latch);
        let put = parse(
            "session_put",
            Some("locked_intent"),
            Some(r#"{"value":"v"}"#),
        )
        .unwrap();
        assert_eq!(put.target.as_deref(), Some("locked_intent"));
        assert_eq!(put.arg(Slot::Value), Some("v"));
        let open = parse(
            "case_open",
            Some("new_case"),
            Some(r#"{"entity":"e","label":"l"}"#),
        )
        .unwrap();
        assert_eq!(open.published_var(), Some("new_case"));
        assert!(parse("case_update", None, Some(r#"{"case":"c","state":"s"}"#)).is_ok());
        assert!(parse(
            "strategy_append",
            None,
            Some(r#"{"case":"c","strategy":"s","outcome":"o"}"#)
        )
        .is_ok());
        assert!(parse(
            "delta_record",
            None,
            Some(r#"{"entities":"e","description":"d"}"#)
        )
        .is_ok());
        assert!(parse("delta_confirm", None, Some(r#"{"handle":"h"}"#)).is_ok());
        assert!(parse("delta_changed", None, Some(r#"{"delta":"d","entity":"e"}"#)).is_ok());
        // items.id=889: the four new operations.
        assert!(parse("case_select", None, Some(r#"{"handle":"h"}"#)).is_ok());
        assert!(parse("case_link", None, Some(r#"{"case":"c","entity":"e"}"#)).is_ok());
        assert!(parse("entity_changed", None, Some(r#"{"case":"c"}"#)).is_ok());
        assert_eq!(
            parse("session_clear", None, None).unwrap().op,
            Op::SessionClear
        );
        assert_eq!(Op::ALL.len(), OP_SPECS.len());
        for op in Op::ALL {
            assert_eq!(Op::parse(op.as_str()), Some(op));
        }
    }

    // -- items.id=889: the four new operations -------------------------------

    #[test]
    fn the_new_operations_have_their_own_slot_errors() {
        // session_clear takes nothing, like the latch.
        assert_eq!(
            parse("session_clear", None, Some("{}")),
            Err(StoreOpError::ArgsNotAllowed("session_clear"))
        );
        assert_eq!(
            parse("session_clear", Some("x"), None),
            Err(StoreOpError::TargetNotAllowed("session_clear"))
        );
        // case_select and delta_confirm need their handle and nothing else.
        for op in ["case_select", "delta_confirm"] {
            assert_eq!(
                parse(op, None, Some("{}")),
                Err(StoreOpError::MissingSlot {
                    op: Op::parse(op).unwrap().as_str(),
                    slot: "handle"
                })
            );
            assert!(matches!(
                parse(op, None, Some(r#"{"handle":"h","case":"c"}"#)),
                Err(StoreOpError::UnknownSlot { .. })
            ));
            assert_eq!(
                parse(op, Some("t"), Some(r#"{"handle":"h"}"#)),
                Err(StoreOpError::TargetNotAllowed(
                    Op::parse(op).unwrap().as_str()
                ))
            );
        }
        // delta_confirm no longer takes ids.
        assert!(matches!(
            parse("delta_confirm", None, Some(r#"{"delta":"d","entity":"e"}"#)),
            Err(StoreOpError::UnknownSlot { .. })
        ));
        // case_link and entity_changed.
        assert_eq!(
            parse("case_link", None, Some(r#"{"case":"c"}"#)),
            Err(StoreOpError::MissingSlot {
                op: "case_link",
                slot: "entity"
            })
        );
        assert_eq!(
            parse("entity_changed", None, Some(r#"{"entity":"e"}"#)),
            Err(StoreOpError::UnknownSlot {
                op: "entity_changed",
                slot: "entity".into()
            }),
            "an entity id is never an argument: the case names it"
        );
    }

    #[test]
    fn only_the_case_writing_operations_are_case_writes_and_only_a_state_is_a_state_var() {
        let writes: Vec<&str> = Op::ALL
            .iter()
            .filter(|o| o.writes_case())
            .map(|o| o.as_str())
            .collect();
        assert_eq!(writes, ["case_update", "strategy_append", "case_link"]);
        let upd = parse("case_update", None, Some(r#"{"case":"c","state":"s"}"#)).unwrap();
        assert_eq!(upd.state_var(), Some("s"));
        let no_state = parse("case_update", None, Some(r#"{"case":"c","outcome":"o"}"#)).unwrap();
        assert_eq!(no_state.state_var(), None);
        let log = parse(
            "strategy_append",
            None,
            Some(r#"{"case":"c","strategy":"s","outcome":"o"}"#),
        )
        .unwrap();
        assert_eq!(log.state_var(), None);
    }

    #[test]
    fn an_unknown_operation_fails_with_its_own_error() {
        assert_eq!(
            parse("drop_table", None, None),
            Err(StoreOpError::UnknownOp("drop_table".into()))
        );
        // Operations match exactly: no case folding, no aliases.
        assert!(matches!(
            parse("Latch", None, None),
            Err(StoreOpError::UnknownOp(_))
        ));
        assert!(matches!(
            parse("", None, None),
            Err(StoreOpError::UnknownOp(_))
        ));
    }

    #[test]
    fn target_rules_are_per_operation() {
        assert_eq!(
            parse("session_put", None, Some(r#"{"value":"v"}"#)),
            Err(StoreOpError::TargetRequired("session_put"))
        );
        assert_eq!(
            parse("latch", Some("x"), None),
            Err(StoreOpError::TargetNotAllowed("latch"))
        );
        assert_eq!(
            parse(
                "case_update",
                Some("x"),
                Some(r#"{"case":"c","state":"s"}"#)
            ),
            Err(StoreOpError::TargetNotAllowed("case_update"))
        );
        assert!(matches!(
            parse("session_put", Some("bad name"), Some(r#"{"value":"v"}"#)),
            Err(StoreOpError::BadTarget(_))
        ));
        // A published target is optional.
        assert_eq!(
            parse("case_open", None, Some(r#"{"entity":"e","label":"l"}"#))
                .unwrap()
                .published_var(),
            None
        );
    }

    #[test]
    fn args_are_a_closed_slot_object() {
        assert_eq!(
            parse("latch", None, Some("{}")),
            Err(StoreOpError::ArgsNotAllowed("latch"))
        );
        assert_eq!(
            parse("session_put", Some("locked_intent"), None),
            Err(StoreOpError::ArgsRequired("session_put"))
        );
        for bad in [
            "not json",
            "[]",
            r#"{"value":1}"#,
            r#"{"value":null}"#,
            r#""x""#,
        ] {
            assert!(
                matches!(
                    parse("session_put", Some("locked_intent"), Some(bad)),
                    Err(StoreOpError::BadArgs(_))
                ),
                "{bad}"
            );
        }
        assert_eq!(
            parse(
                "session_put",
                Some("locked_intent"),
                Some(r#"{"value":"a","value":"b"}"#)
            ),
            Err(StoreOpError::DuplicateSlot {
                slot: "value".into()
            })
        );
        assert_eq!(
            parse(
                "session_put",
                Some("locked_intent"),
                Some(r#"{"value":"a","extra":"b"}"#)
            ),
            Err(StoreOpError::UnknownSlot {
                op: "session_put",
                slot: "extra".into()
            })
        );
        // A slot of another operation is unknown here.
        assert!(matches!(
            parse(
                "session_put",
                Some("locked_intent"),
                Some(r#"{"value":"a","case":"c"}"#)
            ),
            Err(StoreOpError::UnknownSlot { .. })
        ));
        assert_eq!(
            parse("case_open", None, Some(r#"{"entity":"e"}"#)),
            Err(StoreOpError::MissingSlot {
                op: "case_open",
                slot: "label"
            })
        );
        assert_eq!(
            parse("case_update", None, Some(r#"{"case":"c"}"#)),
            Err(StoreOpError::NothingToChange("case_update"))
        );
    }

    /// A slot value is a variable NAME. Anything that could be a literal, a
    /// template or an expression is rejected.
    #[test]
    fn a_slot_value_must_be_a_plain_variable_name() {
        for bad in [
            "",
            "  ",
            "a b",
            "{x}",
            "x+y",
            "x.y",
            "x;y",
            "a\u{0}b",
            &"a".repeat(65),
        ] {
            let args = serde_json::json!({ "value": bad }).to_string();
            assert!(
                matches!(
                    parse("session_put", Some("locked_intent"), Some(&args)),
                    Err(StoreOpError::BadVariable { .. })
                ),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn args_come_out_in_table_order_with_names_trimmed() {
        let o = parse(
            "case_update",
            None,
            Some(r#"{"outcome":" o ","state":"s","case":"c"}"#),
        )
        .unwrap();
        let slots: Vec<&str> = o.args.iter().map(|(s, _)| s.as_str()).collect();
        assert_eq!(slots, ["case", "state", "outcome"]);
        assert_eq!(o.arg(Slot::Outcome), Some("o"));
    }

    /// The slot table as designed, pinned.
    #[test]
    fn the_slot_table_is_pinned() {
        let names = |s: &[Slot]| s.iter().map(|x| x.as_str()).collect::<Vec<_>>();
        let row = |op: Op| {
            let s = op.spec();
            (s.target, names(s.required), names(s.optional))
        };
        assert_eq!(row(Op::Latch), (TargetRule::None, vec![], vec![]));
        assert_eq!(
            row(Op::SessionPut),
            (TargetRule::SessionName, vec!["value"], vec![])
        );
        assert_eq!(
            row(Op::CaseOpen),
            (
                TargetRule::PublishId,
                vec!["entity", "label"],
                vec!["symptoms"]
            )
        );
        assert_eq!(
            row(Op::CaseUpdate),
            (
                TargetRule::None,
                vec!["case"],
                vec![
                    "state",
                    "rung",
                    "resolution_path",
                    "symptoms",
                    "outcome",
                    "durable_facts"
                ]
            )
        );
        assert_eq!(
            row(Op::StrategyAppend),
            (
                TargetRule::None,
                vec!["case", "strategy", "outcome"],
                vec!["user_signal"]
            )
        );
        assert_eq!(
            row(Op::DeltaRecord),
            (
                TargetRule::PublishId,
                vec!["entities", "description"],
                vec!["tags", "case"]
            )
        );
        // items.id=889: delta_confirm takes a handle, not ids.
        assert_eq!(
            row(Op::DeltaConfirm),
            (TargetRule::None, vec!["handle"], vec![])
        );
        assert_eq!(
            row(Op::DeltaChanged),
            (TargetRule::None, vec!["delta", "entity"], vec![])
        );
        assert_eq!(
            row(Op::CaseSelect),
            (TargetRule::None, vec!["handle"], vec![])
        );
        assert_eq!(
            row(Op::CaseLink),
            (TargetRule::None, vec!["case", "entity"], vec![])
        );
        assert_eq!(
            row(Op::EntityChanged),
            (TargetRule::None, vec!["case"], vec![])
        );
        assert_eq!(row(Op::SessionClear), (TargetRule::None, vec![], vec![]));
        // items.id=891: the two Focus-owned entity writers.
        assert_eq!(
            row(Op::EntityFindOrCreate),
            (
                TargetRule::PublishIdRequired,
                vec!["label"],
                vec!["handle", "class"]
            )
        );
        assert_eq!(
            row(Op::EntityFactPut),
            (TargetRule::None, vec!["entity", "field", "value"], vec![])
        );
    }

    #[test]
    fn the_artifact_operations_are_pinned_and_parse() {
        assert_eq!(Op::ALL.len(), OP_SPECS.len());
        let names = |s: &[Slot]| s.iter().map(|x| x.as_str()).collect::<Vec<_>>();
        let spec = Op::ArtifactPut.spec();
        assert_eq!(
            (spec.target, names(spec.required), names(spec.optional)),
            (TargetRule::ArtifactName, vec!["value"], vec![])
        );
        let spec = Op::ArtifactClear.spec();
        assert_eq!(
            (spec.target, names(spec.required), names(spec.optional)),
            (TargetRule::ArtifactName, vec![], vec![])
        );
        let put = parse(
            "artifact_put",
            Some("content_brief"),
            Some(r#"{"value":"brief_text"}"#),
        )
        .unwrap();
        assert_eq!(put.target.as_deref(), Some("content_brief"));
        assert_eq!(put.arg(Slot::Value), Some("brief_text"));
        assert_eq!(put.published_var(), None);
        let clear = parse("artifact_clear", Some("delta_items"), None).unwrap();
        assert_eq!(clear.op, Op::ArtifactClear);
        // The target is required, and clear takes no slots.
        assert_eq!(
            parse("artifact_put", None, Some(r#"{"value":"v"}"#)),
            Err(StoreOpError::TargetRequired("artifact_put"))
        );
        assert_eq!(
            parse("artifact_clear", None, None),
            Err(StoreOpError::TargetRequired("artifact_clear"))
        );
        assert_eq!(
            parse("artifact_clear", Some("delta_items"), Some("{}")),
            Err(StoreOpError::ArgsNotAllowed("artifact_clear"))
        );
        assert_eq!(
            parse("artifact_put", Some("content_brief"), None),
            Err(StoreOpError::ArgsRequired("artifact_put"))
        );
        assert!(matches!(
            parse(
                "artifact_put",
                Some("content_brief"),
                Some(r#"{"value":"v","case":"c"}"#)
            ),
            Err(StoreOpError::UnknownSlot { .. })
        ));
        // Neither touches entities or the case.
        for o in [Op::ArtifactPut, Op::ArtifactClear] {
            assert!(!o.is_entity_op() && !o.takes_entity_ids() && !o.writes_case());
        }
        assert!(Op::ArtifactPut.requires_collection_consent());
        assert!(!Op::ArtifactClear.requires_collection_consent());
    }

    // -- composition checks ---------------------------------------------------

    fn base(id: &str) -> ActionDefinition {
        ActionDefinition {
            action_id: id.to_owned(),
            display_name: id.to_owned(),
            guide_id: "g".to_owned(),
            task_type: "general".to_owned(),
            routing_tier: ExternalAccess::LocalOnly,
            step_type: StepType::Generate,
            output_var: None,
            prompt_template: String::new(),
            field_requirements: HashMap::new(),
            options_override: HashMap::new(),
            external_access_override: None,
            requires_user_handoff: false,
            revisitable: false,
            block_stable_id: None,
            block_customization: None,
            schedule_trigger: None,
            guard: None,
            declared_inputs: Vec::new(),
            store_op: None,
            and_guards: Vec::new(),
            requirement: None,
        }
    }

    fn model(id: &str, out: &str) -> (ActionDefinition, Option<String>) {
        let mut s = base(id);
        s.output_var = Some(out.to_owned());
        s.prompt_template = "x".to_owned();
        (s, Some(out.to_owned()))
    }

    /// An entity_find_or_create row that publishes `var`. Its label comes from the
    /// variable `l`, which the test supplies with model("l", "l") before it.
    fn ent(var: &str) -> (ActionDefinition, Option<String>) {
        writer(
            &format!("ent_{var}"),
            "entity_find_or_create",
            Some(var),
            Some(r#"{"label":"l"}"#),
            None,
        )
    }

    fn writer(
        id: &str,
        op: &str,
        target: Option<&str>,
        args: Option<&str>,
        guard: Option<Guard>,
    ) -> (ActionDefinition, Option<String>) {
        let mut s = base(id);
        s.store_op = StoreOp::from_columns(Some(op), target, args).expect("parses");
        s.guard = guard;
        let v = s
            .store_op
            .as_ref()
            .and_then(|o| o.published_var())
            .map(str::to_owned);
        (s, v)
    }

    fn check(rows: Vec<(ActionDefinition, Option<String>)>) -> Result<(), StoreOpError> {
        check_with(rows, CollectionPosture::FocusLevel)
    }

    fn check_with(
        rows: Vec<(ActionDefinition, Option<String>)>,
        posture: CollectionPosture,
    ) -> Result<(), StoreOpError> {
        let (steps, vars): (Vec<_>, Vec<_>) = rows.into_iter().unzip();
        check_composition(&steps, &vars, posture)
    }

    fn put(target: &str, var: &str) -> (ActionDefinition, Option<String>) {
        writer(
            "put",
            "session_put",
            Some(target),
            Some(&format!(r#"{{"value":"{var}"}}"#)),
            None,
        )
    }

    #[test]
    fn a_session_put_to_a_session_name_from_an_earlier_variable_loads() {
        assert_eq!(
            check(vec![model("a", "v"), put("locked_intent", "v")]),
            Ok(())
        );
        assert_eq!(
            check(vec![model("a", "v"), put("active_case_id", "v")]),
            Ok(())
        );
    }

    #[test]
    fn session_put_cannot_target_the_latch_a_derived_name_or_an_unknown_name() {
        assert!(matches!(
            check(vec![model("a", "v"), put("write_mode", "v")]),
            Err(StoreOpError::WriteModeNotPutable { .. })
        ));
        for derived in [
            "case_context",
            "delta_context",
            "open_ticket_state",
            "open_tickets_brief",
        ] {
            assert!(
                matches!(
                    check(vec![model("a", "v"), put(derived, "v")]),
                    Err(StoreOpError::DerivedSessionName { .. })
                ),
                "{derived}"
            );
        }
        assert!(matches!(
            check(vec![model("a", "v"), put("anything_else", "v")]),
            Err(StoreOpError::UnknownSessionName { .. })
        ));
    }

    fn artifact_put(target: &str, var: &str) -> (ActionDefinition, Option<String>) {
        writer(
            "aput",
            "artifact_put",
            Some(target),
            Some(&format!(r#"{{"value":"{var}"}}"#)),
            None,
        )
    }

    // -- items.id=387: handoff_ingest and the hand-off names ----------------------

    fn ingest(
        target: &str,
        guard: Option<Guard>,
        and_guards: Vec<Guard>,
    ) -> (ActionDefinition, Option<String>) {
        let (mut s, v) = writer("ingest", "handoff_ingest", Some(target), None, guard);
        s.and_guards = and_guards;
        (s, v)
    }

    fn guard_on(var: &str, condition: Condition) -> Guard {
        Guard {
            var: var.to_owned(),
            condition,
        }
    }

    #[test]
    fn handoff_ingest_takes_a_target_and_no_slots() {
        let o = parse("handoff_ingest", Some("draft_ref"), None).unwrap();
        assert_eq!(o.op, Op::HandoffIngest);
        assert_eq!(o.published_var(), Some("draft_ref"));
        assert!(o.args.is_empty());
        assert_eq!(
            parse("handoff_ingest", None, None),
            Err(StoreOpError::TargetRequired("handoff_ingest"))
        );
        assert_eq!(
            parse("handoff_ingest", Some("r"), Some("{}")),
            Err(StoreOpError::ArgsNotAllowed("handoff_ingest"))
        );
        // The paste is NOT a slot, under any name: there is nothing to configure.
        assert_eq!(
            parse(
                "handoff_ingest",
                Some("r"),
                Some(r#"{"value":"user_input"}"#)
            ),
            Err(StoreOpError::ArgsNotAllowed("handoff_ingest"))
        );
        assert!(matches!(
            parse("handoff_ingest", Some("bad name"), None),
            Err(StoreOpError::BadTarget(_))
        ));
        assert!(!Op::HandoffIngest.is_entity_op());
        assert!(!Op::HandoffIngest.takes_entity_ids());
        assert!(!Op::HandoffIngest.writes_case());
        assert!(Op::HandoffIngest.requires_collection_consent());
        assert_eq!(Op::ALL.len(), 19);
        assert_eq!(Op::ALL.len(), OP_SPECS.len());
    }

    #[test]
    fn handoff_ingest_loads_only_guarded_on_pending_being_present() {
        let present = || Some(guard_on(HANDOFF_PENDING, Condition::Present));
        for posture in [CollectionPosture::FocusLevel, CollectionPosture::PerField] {
            assert_eq!(
                check_with(vec![ingest("draft_ref", present(), vec![])], posture),
                Ok(()),
                "guard"
            );
            // The clause list counts too: the clauses are ANDed.
            assert_eq!(
                check_with(
                    vec![ingest(
                        "draft_ref",
                        None,
                        vec![guard_on(HANDOFF_PENDING, Condition::Present)]
                    )],
                    posture
                ),
                Ok(()),
                "clause"
            );
        }
        // Unguarded, or guarded on anything else, or on the wrong condition.
        for (g, ands) in [
            (None, vec![]),
            (Some(guard_on("write_mode", Condition::Present)), vec![]),
            (Some(guard_on(HANDOFF_PENDING, Condition::Absent)), vec![]),
            (
                Some(guard_on(
                    HANDOFF_PENDING,
                    Condition::Eq("h_000000000000".into()),
                )),
                vec![],
            ),
            (Some(guard_on(HANDOFF_RECEIVED, Condition::Present)), vec![]),
            (None, vec![guard_on(HANDOFF_PENDING, Condition::Absent)]),
        ] {
            assert!(
                matches!(
                    check(vec![ingest("draft_ref", g.clone(), ands.clone())]),
                    Err(StoreOpError::HandoffIngestUnguarded { .. })
                ),
                "{g:?} {ands:?}"
            );
        }
    }

    #[test]
    fn the_published_handle_name_cannot_collide() {
        let present = || Some(guard_on(HANDOFF_PENDING, Condition::Present));
        for bad in [
            "handoff_pending",
            "handoff_received",
            "content_brief",
            "user_input",
        ] {
            assert!(
                matches!(
                    check(vec![ingest(bad, present(), vec![])]),
                    Err(StoreOpError::PublishedNameCollision { .. })
                ),
                "{bad}"
            );
        }
    }

    #[test]
    fn the_hand_off_names_are_never_a_session_put_target_or_a_writer_argument() {
        for name in HANDOFF_NAMES {
            assert!(
                matches!(
                    check(vec![model("a", "v"), put(name, "v")]),
                    Err(StoreOpError::HandoffNameNotSessionPutable { .. })
                ),
                "target {name}"
            );
            // As an argument, even with the guard an optional name would need.
            let (mut row, v) = writer(
                "copy",
                "session_put",
                Some("locked_intent"),
                Some(&format!(r#"{{"value":"{name}"}}"#)),
                Some(guard_on(name, Condition::Present)),
            );
            row.and_guards = Vec::new();
            assert!(
                matches!(
                    check(vec![(row, v)]),
                    Err(StoreOpError::ArgumentHandoffName { .. })
                ),
                "argument {name}"
            );
        }
    }

    /// items.id=897: the launch names are answered before the run, so a Focus row
    /// never writes one and never hands one to a writer.
    #[test]
    fn the_launch_names_are_never_a_session_put_target_or_a_writer_argument() {
        for name in LAUNCH_NAMES {
            assert!(
                matches!(
                    check(vec![model("a", "v"), put(name, "v")]),
                    Err(StoreOpError::LaunchNameNotSessionPutable { .. })
                ),
                "target {name}"
            );
            let (mut row, v) = writer(
                "copy",
                "session_put",
                Some("locked_intent"),
                Some(&format!(r#"{{"value":"{name}"}}"#)),
                None,
            );
            row.and_guards = Vec::new();
            assert!(
                matches!(
                    check(vec![(row, v)]),
                    Err(StoreOpError::ArgumentLaunchName { .. })
                ),
                "argument {name}"
            );
        }
        // Nor an artifact_put target.
        for name in LAUNCH_NAMES {
            assert!(
                matches!(
                    check(vec![
                        model("a", "v"),
                        writer(
                            "ap",
                            "artifact_put",
                            Some(name),
                            Some(r#"{"value":"v"}"#),
                            None
                        )
                    ]),
                    Err(StoreOpError::UnknownArtifactName { .. })
                ),
                "artifact {name}"
            );
        }
    }

    /// items.id=895: an artifact_put to each artifact name, from an earlier
    /// variable, loads, in a per_field Focus as well (no entity involved), and so
    /// does a clear.
    #[test]
    fn an_artifact_put_and_clear_to_an_artifact_name_load() {
        for name in crate::conductor::bound_spec::ARTIFACT_NAMES {
            for posture in [CollectionPosture::FocusLevel, CollectionPosture::PerField] {
                assert_eq!(
                    check_with(vec![model("a", "v"), artifact_put(name, "v")], posture),
                    Ok(()),
                    "{name}"
                );
                assert_eq!(
                    check_with(
                        vec![writer("aclr", "artifact_clear", Some(name), None, None)],
                        posture
                    ),
                    Ok(()),
                    "{name}"
                );
            }
        }
    }

    #[test]
    fn an_artifact_writer_cannot_target_anything_but_an_artifact_name() {
        for bad in [
            "locked_intent",
            "active_case_id",
            "write_mode",
            "case_context",
            "anything_else",
        ] {
            for row in [
                artifact_put(bad, "v"),
                writer("aclr", "artifact_clear", Some(bad), None, None),
            ] {
                let e = check(vec![model("a", "v"), row]).unwrap_err();
                assert!(
                    matches!(&e, StoreOpError::UnknownArtifactName { name, .. } if name == bad),
                    "{bad}: {e:?}"
                );
            }
        }
    }

    #[test]
    fn session_put_cannot_target_an_artifact_name() {
        for name in crate::conductor::bound_spec::ARTIFACT_NAMES {
            assert!(
                matches!(
                    check(vec![model("a", "v"), put(name, "v")]),
                    Err(StoreOpError::ArtifactNotSessionName { .. })
                ),
                "{name}"
            );
        }
    }

    /// An artifact name is read through a template and never copied back: it is
    /// refused as the argument of any writer.
    #[test]
    fn an_artifact_name_is_never_a_writer_argument() {
        for name in crate::conductor::bound_spec::ARTIFACT_NAMES {
            for rows in [
                vec![artifact_put("delta_items", name)],
                vec![put("locked_intent", name)],
                vec![
                    model("c", "c"),
                    writer(
                        "w",
                        "strategy_append",
                        None,
                        Some(&format!(
                            r#"{{"case":"c","strategy":"{name}","outcome":"{name}"}}"#
                        )),
                        None,
                    ),
                ],
            ] {
                let e = check(rows).unwrap_err();
                assert!(
                    matches!(&e, StoreOpError::ArgumentArtifact { var, .. } if var == name),
                    "{name}: {e:?}"
                );
            }
        }
    }

    #[test]
    fn an_artifact_put_value_must_come_from_an_earlier_variable() {
        assert!(matches!(
            check(vec![artifact_put("content_brief", "v")]),
            Err(StoreOpError::ArgumentUnknown { .. })
        ));
        assert!(matches!(
            check(vec![artifact_put("content_brief", "v"), model("a", "v")]),
            Err(StoreOpError::ArgumentFromLaterRow { .. })
        ));
        assert!(matches!(
            check(vec![
                model("a", "v"),
                artifact_put("content_brief", "user_input")
            ]),
            Err(StoreOpError::ArgumentNotAllowed { .. })
        ));
    }

    #[test]
    fn the_latch_is_reachable_only_through_the_latch_operation() {
        // write_mode as an ARGUMENT of any other operation is refused too.
        let rows = vec![writer(
            "w",
            "session_put",
            Some("locked_intent"),
            Some(r#"{"value":"write_mode"}"#),
            None,
        )];
        assert!(matches!(
            check(rows),
            Err(StoreOpError::ArgumentNotAllowed { .. })
        ));
        let ok = vec![writer("l", "latch", None, None, None)];
        assert_eq!(check(ok), Ok(()));
    }

    #[test]
    fn an_argument_must_come_from_an_earlier_row() {
        // Produced by a LATER row.
        let e = check(vec![put("locked_intent", "v"), model("a", "v")]).unwrap_err();
        assert!(
            matches!(e, StoreOpError::ArgumentFromLaterRow { .. }),
            "{e}"
        );
        assert!(e
            .to_string()
            .contains("cannot sit before the data it needs"));
        // Produced by nobody.
        assert!(matches!(
            check(vec![model("a", "v"), put("locked_intent", "ghost")]),
            Err(StoreOpError::ArgumentUnknown { .. })
        ));
        // A store row is not a producer of its own argument.
        assert!(matches!(
            check(vec![put("locked_intent", "locked_intent_x")]),
            Err(StoreOpError::ArgumentUnknown { .. })
        ));
    }

    #[test]
    fn a_published_id_is_data_for_later_rows_only() {
        let open = || {
            writer(
                "open",
                "case_open",
                Some("new_case"),
                Some(r#"{"entity":"e","label":"l"}"#),
                None,
            )
        };
        let update = || {
            writer(
                "upd",
                "case_update",
                None,
                Some(r#"{"case":"new_case","state":"s"}"#),
                None,
            )
        };
        assert_eq!(
            check(vec![
                model("l", "l"),
                ent("e"),
                model("s", "s"),
                open(),
                update()
            ]),
            Ok(())
        );
        assert!(matches!(
            check(vec![
                model("l", "l"),
                ent("e"),
                model("s", "s"),
                update(),
                open()
            ]),
            Err(StoreOpError::ArgumentFromLaterRow { .. })
        ));
    }

    #[test]
    fn a_published_name_may_not_collide() {
        let open = |target| {
            writer(
                "open",
                "case_open",
                Some(target),
                Some(r#"{"entity":"e","label":"l"}"#),
                None,
            )
        };
        for bad in ["active_case_id", "write_mode", "user_input"] {
            assert!(
                matches!(
                    check(vec![model("e", "e"), model("l", "l"), open(bad)]),
                    Err(StoreOpError::PublishedNameCollision { .. })
                ),
                "{bad}"
            );
        }
        // Another row already produces that name.
        assert!(matches!(
            check(vec![model("e", "e"), model("l", "l"), open("l")]),
            Err(StoreOpError::PublishedNameCollision { .. })
        ));
    }

    #[test]
    fn derived_names_and_system_tokens_are_not_arguments() {
        let w = |var: &str| {
            writer(
                "w",
                "session_put",
                Some("locked_intent"),
                Some(&format!(r#"{{"value":"{var}"}}"#)),
                None,
            )
        };
        assert!(matches!(
            check(vec![w("case_context")]),
            Err(StoreOpError::ArgumentDerived { .. })
        ));
        for token in [
            "user_input",
            "persona_context",
            "previous_output",
            "voice_profile",
            "focus_context",
        ] {
            assert!(
                matches!(
                    check(vec![w(token)]),
                    Err(StoreOpError::ArgumentNotAllowed { .. })
                ),
                "{token}"
            );
        }
    }

    #[test]
    fn an_optional_session_argument_needs_a_guard_that_requires_it() {
        let upd = |guard| {
            writer(
                "upd",
                "case_update",
                None,
                Some(r#"{"case":"active_case_id","state":"s"}"#),
                guard,
            )
        };
        let g = |op: &str, v: Option<&str>| {
            Guard::from_columns(Some("active_case_id"), Some(op), v).unwrap()
        };
        assert!(matches!(
            check(vec![model("a", "s"), upd(None)]),
            Err(StoreOpError::OptionalArgumentUnguarded { .. })
        ));
        // absent does not cover it: the row would run exactly when it is unset.
        assert!(matches!(
            check(vec![model("a", "s"), upd(g("absent", None))]),
            Err(StoreOpError::OptionalArgumentUnguarded { .. })
        ));
        // A guard on a different variable does not cover it.
        let other = Guard::from_columns(Some("s"), Some("present"), None).unwrap();
        assert!(matches!(
            check(vec![model("a", "s"), upd(other)]),
            Err(StoreOpError::OptionalArgumentUnguarded { .. })
        ));
        assert_eq!(
            check(vec![model("a", "s"), upd(g("present", None))]),
            Ok(())
        );
        assert_eq!(
            check(vec![model("a", "s"), upd(g("ne", Some("zz")))]),
            Ok(())
        );
        // write_mode is Always-present but is never an argument (above).
    }

    #[test]
    fn a_composition_without_writers_always_passes() {
        assert_eq!(check(vec![model("a", "v"), model("b", "w")]), Ok(()));
    }

    // -- items.id=891: the Focus-owned entity writers --------------------------

    /// The five rows that take an entity id, each with its slot name and the
    /// variable it names ("e").
    fn rows_taking_an_entity() -> Vec<(&'static str, (ActionDefinition, Option<String>))> {
        vec![
            (
                "case_open",
                writer(
                    "w",
                    "case_open",
                    None,
                    Some(r#"{"entity":"e","label":"l"}"#),
                    None,
                ),
            ),
            (
                "case_link",
                writer(
                    "w",
                    "case_link",
                    None,
                    Some(r#"{"case":"c","entity":"e"}"#),
                    None,
                ),
            ),
            (
                "delta_changed",
                writer(
                    "w",
                    "delta_changed",
                    None,
                    Some(r#"{"delta":"c","entity":"e"}"#),
                    None,
                ),
            ),
            (
                "delta_record",
                writer(
                    "w",
                    "delta_record",
                    None,
                    Some(r#"{"entities":"e","description":"l"}"#),
                    None,
                ),
            ),
            (
                "entity_fact_put",
                writer(
                    "w",
                    "entity_fact_put",
                    None,
                    Some(r#"{"entity":"e","field":"l","value":"l"}"#),
                    None,
                ),
            ),
        ]
    }

    /// An entity slot names the id an EARLIER entity_find_or_create row published,
    /// for every operation that takes one, and nothing else: not a model output,
    /// not a later row's id, not another writer's published id, not a seeded name.
    #[test]
    fn every_entity_slot_must_name_an_earlier_published_entity_id() {
        for (name, w) in rows_taking_an_entity() {
            // The published id of an earlier entity row: loads.
            assert_eq!(
                check(vec![model("l", "l"), model("c", "c"), ent("e"), w.clone()]),
                Ok(()),
                "{name}"
            );
            // The same name produced by a MODEL step: refused, the id is the
            // model's text.
            assert!(
                matches!(
                    check(vec![
                        model("l", "l"),
                        model("c", "c"),
                        model("e", "e"),
                        w.clone()
                    ]),
                    Err(StoreOpError::EntitySlotNotPublished { .. })
                ),
                "{name}: a model output as an entity id"
            );
            // Published by a LATER entity row: refused.
            assert!(
                matches!(
                    check(vec![model("l", "l"), model("c", "c"), w.clone(), ent("e")]),
                    Err(StoreOpError::EntitySlotNotPublished { .. })
                ),
                "{name}: a later row's id"
            );
            // Nothing produces it.
            assert!(
                matches!(
                    check(vec![model("l", "l"), model("c", "c"), w.clone()]),
                    Err(StoreOpError::EntitySlotNotPublished { .. })
                ),
                "{name}: an unknown name"
            );
        }
    }

    /// A published CASE id, a seeded name and a system token cannot stand in for an
    /// entity id either.
    #[test]
    fn a_case_id_a_seeded_name_or_a_token_is_not_an_entity_id() {
        let link_with = |var: &str| {
            writer(
                "link",
                "case_link",
                None,
                Some(&format!(r#"{{"case":"c","entity":"{var}"}}"#)),
                None,
            )
        };
        let open = || {
            writer(
                "open",
                "case_open",
                Some("new_case"),
                Some(r#"{"entity":"e","label":"l"}"#),
                None,
            )
        };
        for var in ["new_case", "active_case_id", "locked_intent", "user_input"] {
            assert!(
                matches!(
                    check(vec![
                        model("l", "l"),
                        model("c", "c"),
                        ent("e"),
                        open(),
                        link_with(var)
                    ]),
                    Err(StoreOpError::EntitySlotNotPublished { .. })
                ),
                "{var}"
            );
        }
    }

    /// Only an entity_find_or_create row publishes an entity id: another entity row
    /// may publish a second device (the affected one), and each slot picks its own.
    #[test]
    fn two_entity_rows_publish_two_ids() {
        let link = writer(
            "link",
            "case_link",
            None,
            Some(r#"{"case":"new_case","entity":"affected"}"#),
            None,
        );
        let open = writer(
            "open",
            "case_open",
            Some("new_case"),
            Some(r#"{"entity":"e","label":"l"}"#),
            None,
        );
        assert_eq!(
            check(vec![model("l", "l"), ent("e"), ent("affected"), open, link]),
            Ok(())
        );
    }

    /// The entity writers save the user's equipment, so they are for a focus_level
    /// Focus only: a per_field Focus is never consulted by the consent gate.
    #[test]
    fn the_entity_writers_are_refused_in_a_per_field_focus() {
        let put = writer(
            "put",
            "entity_fact_put",
            None,
            Some(r#"{"entity":"e","field":"l","value":"l"}"#),
            None,
        );
        // The first entity row of the Focus is the one named.
        assert!(matches!(
            check_with(
                vec![model("l", "l"), ent("e"), put.clone()],
                CollectionPosture::PerField
            ),
            Err(StoreOpError::EntityOpNeedsFocusLevel {
                op: "entity_find_or_create",
                ..
            })
        ));
        assert!(matches!(
            check_with(vec![model("l", "l"), ent("e")], CollectionPosture::PerField),
            Err(StoreOpError::EntityOpNeedsFocusLevel { .. })
        ));
        assert_eq!(
            check_with(
                vec![model("l", "l"), ent("e"), put],
                CollectionPosture::FocusLevel
            ),
            Ok(())
        );
        // A Focus with no entity writer loads in either posture.
        assert_eq!(
            check_with(vec![model("l", "l")], CollectionPosture::PerField),
            Ok(())
        );
    }

    /// entity_find_or_create must publish its id (every later entity slot names
    /// it), and its handle slot is model output, never a bound name or an id this
    /// pass published.
    #[test]
    fn find_or_create_needs_a_target_and_takes_a_model_handle_only() {
        assert_eq!(
            parse("entity_find_or_create", None, Some(r#"{"label":"l"}"#)),
            Err(StoreOpError::TargetRequired("entity_find_or_create"))
        );
        let with_handle = |handle: &str| {
            writer(
                "ent2",
                "entity_find_or_create",
                Some("e2"),
                Some(&format!(r#"{{"label":"l","handle":"{handle}"}}"#)),
                None,
            )
        };
        assert_eq!(
            check(vec![model("l", "l"), model("h", "h"), with_handle("h")]),
            Ok(())
        );
        for bad in ["active_case_id", "e"] {
            assert!(
                matches!(
                    check(vec![model("l", "l"), ent("e"), with_handle(bad)]),
                    Err(StoreOpError::HandleNotFromModel { .. })
                ),
                "{bad}"
            );
        }
        // Slot names: handle and class are optional, label is required.
        assert!(parse(
            "entity_find_or_create",
            Some("e"),
            Some(r#"{"label":"l","handle":"h","class":"c"}"#)
        )
        .is_ok());
        assert!(matches!(
            parse(
                "entity_find_or_create",
                Some("e"),
                Some(r#"{"handle":"h"}"#)
            ),
            Err(StoreOpError::MissingSlot { slot: "label", .. })
        ));
        assert!(matches!(
            parse(
                "entity_find_or_create",
                Some("e"),
                Some(r#"{"label":"l","entity":"x"}"#)
            ),
            Err(StoreOpError::UnknownSlot { .. })
        ));
        // entity_fact_put takes no target and needs all three slots.
        assert_eq!(
            parse(
                "entity_fact_put",
                Some("t"),
                Some(r#"{"entity":"e","field":"f","value":"v"}"#)
            ),
            Err(StoreOpError::TargetNotAllowed("entity_fact_put"))
        );
        assert!(matches!(
            parse(
                "entity_fact_put",
                None,
                Some(r#"{"entity":"e","field":"f"}"#)
            ),
            Err(StoreOpError::MissingSlot { slot: "value", .. })
        ));
    }

    /// The new operations' own classification: both are entity writers, both
    /// require consent, and the ones that carry an entity id are exactly the five.
    #[test]
    fn the_entity_classifications_are_exactly_these() {
        let entity_ops: Vec<&str> = Op::ALL
            .iter()
            .filter(|o| o.is_entity_op())
            .map(|o| o.as_str())
            .collect();
        assert_eq!(entity_ops, ["entity_find_or_create", "entity_fact_put"]);
        let with_ids: Vec<&str> = Op::ALL
            .iter()
            .filter(|o| o.takes_entity_ids())
            .map(|o| o.as_str())
            .collect();
        assert_eq!(
            with_ids,
            [
                "case_open",
                "delta_record",
                "delta_changed",
                "case_link",
                "entity_fact_put"
            ]
        );
        assert!(Op::EntityFindOrCreate.requires_collection_consent());
        assert!(Op::EntityFactPut.requires_collection_consent());
        assert!(!Op::EntityFindOrCreate.writes_case() && !Op::EntityFactPut.writes_case());
        assert_eq!(Op::EntityFindOrCreate.as_str(), "entity_find_or_create");
        assert_eq!(
            parse("entity_find_or_create", Some("e"), Some(r#"{"label":"l"}"#))
                .unwrap()
                .published_var(),
            Some("e")
        );
    }
}
