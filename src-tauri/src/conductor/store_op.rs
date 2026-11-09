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
// and nothing here lets a tainted value reach an external prompt: that is still
// STORE_NAMES_EXTERNAL_ALLOWED (declared_inputs.rs). A published id is derived
// from the run, not from content, and is not tainted.

use std::collections::HashSet;

use serde::de::{MapAccess, Visitor};
use serde::{Deserialize, Deserializer};

use crate::conductor::bound_spec::{self, Presence, Source, WRITE_MODE};
use crate::conductor::guard::Condition;
use crate::conductor::tokens::{is_system_token, ActionDefinition};

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
}

impl Op {
    /// The closed set, in documentation order.
    pub const ALL: [Op; 12] = [
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
        }
    }

    /// True for the operations that write the CASE itself (its record, its
    /// strategy log or its entity links). A terminal case takes none of them, so
    /// the close-ticket lint (guard.rs) keeps every one of them before a closing
    /// case_update on any path.
    pub fn writes_case(self) -> bool {
        matches!(self, Op::CaseUpdate | Op::StrategyAppend | Op::CaseLink)
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
    /// handle. The value of a model step's output, resolved store-side.
    Handle,
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
    #[error("unknown store_op '{0}' (expected one of latch, session_put, case_open, case_update, strategy_append, delta_record, delta_confirm, delta_changed, case_select, case_link, entity_changed, session_clear)")]
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
            (TargetRule::SessionName, None) => return Err(StoreOpError::TargetRequired(name)),
            (TargetRule::SessionName, Some(t)) | (TargetRule::PublishId, Some(t)) => {
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
    /// delta_record with a store_target), else None.
    pub fn published_var(&self) -> Option<&str> {
        match self.op {
            Op::CaseOpen | Op::DeltaRecord => self.target.as_deref(),
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
pub fn check_composition(
    steps: &[ActionDefinition],
    row_vars: &[Option<String>],
) -> Result<(), StoreOpError> {
    debug_assert_eq!(steps.len(), row_vars.len());
    for (i, step) in steps.iter().enumerate() {
        let Some(op) = &step.store_op else { continue };
        let action_id = step.action_id.clone();

        // The target of session_put is a registered session name, and the latch
        // is out of its reach.
        if op.op == Op::SessionPut {
            let name = op.target.as_deref().unwrap_or("");
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
            if let Some(s) = bound_spec::spec(var) {
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
        }
    }

    fn model(id: &str, out: &str) -> (ActionDefinition, Option<String>) {
        let mut s = base(id);
        s.output_var = Some(out.to_owned());
        s.prompt_template = "x".to_owned();
        (s, Some(out.to_owned()))
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
        let (steps, vars): (Vec<_>, Vec<_>) = rows.into_iter().unzip();
        check_composition(&steps, &vars)
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
                model("e", "e"),
                model("l", "l"),
                model("s", "s"),
                open(),
                update()
            ]),
            Ok(())
        );
        assert!(matches!(
            check(vec![
                model("e", "e"),
                model("l", "l"),
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
}
