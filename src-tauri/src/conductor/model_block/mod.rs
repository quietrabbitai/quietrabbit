// src-tauri/src/conductor/model_block/mod.rs
//
// items.id=894 part 1 (decisions.id=941 point 2, decisions.id=942): MODEL BLOCKS.
//
// A model block is a declared spec in the same registry as the pure blocks
// (conductor::block_dispatch), but it has no handler. At LOAD a composition row
// that cites one is parsed (typed, deny_unknown_fields) and EXPANDED into an
// ordinary ActionDefinition: a model-call step with a generated prompt template,
// its own output_var and a `requirement` field naming what it needs. From there
// it is an ordinary step in every respect: Gate 1, the spend gate, Gate 2, Gate 3
// and the declared-inputs lint all apply on the one existing path, and a spec
// never gets provider access (decisions.id=942 point 1). The expanded step has
// block_stable_id None on purpose: execute_step() dispatches any Some to a pure
// handler, extraction treats a block verdict as non-conversation, and the block
// placement lint forbids a block last or before an external step. A model block's
// output is real content, so none of that applies.
//
// Four kinds of block exist (BlockKind): Pure (a BlockHandler over the
// TaskTrack, cb-06), Model (this module: cb-12 draft, cb-13 voice, cb-14 revise),
// Render (items.id=387: cb-15, the manual hand-off starter, conductor::
// handoff_render: a deterministic render of text from declared inputs, no model
// call) and Seed (a bound-name producer; none is registered yet, the first belong
// to B7a, and a seed's contract lives with items.id=886's bound-name table).
//
// WHAT LOAD ENFORCES FOR A MODEL BLOCK (each with its own message):
//   * the configuration parses, with closed vocabularies and no unknown key;
//   * EXTERNAL_MODEL_BLOCKS_ALLOWED (below) is false: nothing but destination
//     local at tier local_only loads;
//   * the HOSTED RULE: a block whose range allows hosted_api names neither
//     {user_input} nor {persona_context}, in its declared inputs or its generated
//     template (decisions.id=942 point 1; the draft block's external step receives
//     only the brief, voice guidance and abstracted context);
//   * the generated template names exactly the declared inputs;
//   * authored text (every phrase and document-type field) contains no {token}.
//
// WHAT IS DATA AND WHAT IS CODE: every sentence a block emits lives in
// block_data/document_types.json (document_type.rs). A block's expand() holds
// only the skeleton: the order of the phrases and where each declared variable
// goes. Wording is tuned by editing the data file.
//
// SETTINGS ARE APPLIED WHEN A STEP IS EXPANDED. expand() is pure and takes the
// resolved settings; LOAD calls it with the document type's shipped defaults. A
// user's saved adjustments and a per-run change (document_type::resolve_settings)
// re-expand the step when a run starts (items.id=897, conductor::run_settings), with
// the run's selected document type (ExpandInput::document_type, which replaces the
// type the row names) and the mode profile of its launch answer
// (ExpandInput::envelope_override, applied through tighten() after the Focus-ceiling
// clamp). The parsed row is kept for that (ModelOrigin, on the FocusDefinition).
//
// The executor reads `requirement` through the model resolver (items.id=896).

pub mod brief;
pub mod config;
pub mod document_type;

pub(crate) mod draft;
pub(crate) mod elicit;
pub(crate) mod revise;
pub(crate) mod voice;

#[cfg(test)]
mod admission_tests;

use std::collections::{BTreeSet, HashMap};

use serde_json::Value;

use crate::conductor::executor::find_tokens;
use crate::conductor::guard::{Condition, Guard};
use crate::conductor::tokens::{ActionDefinition, ExternalAccess, StepType};

use self::config::{tighten, CommonConfig, Destination, EnvelopeOverride, ModelRequirement};
use self::document_type::{Settings, StepParams, WordRange};

/// INTERIM PRIVACY INVARIANT (decisions.id=942 point 1, items.id=894). While this
/// is false, LOAD refuses a model block whose range names any destination but
/// local or any tier above local_only: hosted_api needs the outbound review of a
/// rendered external prompt (items.id=884). One boolean covers both external
/// destinations. manual_handoff as a MODEL block's destination also waits for it,
/// because nothing selects that destination until the run plan (items.id=901) and
/// B10. The hand-off itself is built (items.id=387): its outbound render (cb-15) and
/// its return operation (handoff_ingest) are cited by a Focus row directly and need
/// no model block, so this flag does not govern them. Flip this to true at B10, after
/// items.id=884 lands: it is the only line that changes. See also
/// declared_inputs::STORE_NAMES_EXTERNAL_ALLOWED.
pub const EXTERNAL_MODEL_BLOCKS_ALLOWED: bool = false;

/// The guide every model block's step names. guide_id never injects guide text
/// into a prompt (the guides are not read into templates), so it is a label.
pub const MODEL_BLOCK_GUIDE_ID: &str = "writing-voice";

/// What kind of block a stable id is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockKind {
    /// A BlockHandler over the TaskTrack: no model call, always local.
    Pure,
    /// A ModelBlockSpec: expands to an ordinary model-call step at LOAD.
    Model,
    /// items.id=387: a deterministic RENDER of text from the row's declared inputs
    /// (cb-15). It makes no model call and has no BlockHandler (a handler sees only
    /// the TaskTrack, a render also reads the seeded declared inputs). Its row is
    /// a block row to the registry and an EXTERNAL step to the declared-inputs
    /// lint (conductor::handoff_render).
    Render,
    /// A bound-name producer. None registered yet.
    Seed,
}

/// The block-specific half of a parsed row.
#[derive(Debug, Clone, PartialEq)]
pub enum BlockSpec {
    Draft(draft::DraftSpec),
    Voice(voice::VoiceSpec),
    Revise(revise::ReviseSpec),
    /// items.id=903: pc-03 in its one-round form.
    Elicit(elicit::ElicitSpec),
}

/// A parsed, validated customization JSON of a model block.
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedModelBlock {
    pub common: CommonConfig,
    pub spec: BlockSpec,
}

/// What expand() needs beyond the parsed row.
pub struct ExpandInput<'a> {
    /// The composition row's action_id.
    pub action_id: &'a str,
    /// The Focus row's max_routing_tier: the range is clamped to it at LOAD.
    pub focus_ceiling: ExternalAccess,
    /// The resolved settings. None means the document type's shipped defaults,
    /// which is what LOAD uses.
    pub settings: Option<&'a Settings>,
    /// EXTERNAL_MODEL_BLOCKS_ALLOWED, a parameter so a test can run both ways.
    pub external_allowed: bool,
    /// items.id=897: the run's selected document type. When set it replaces the
    /// document type the row's own configuration names, so the generated template
    /// and the resolver's profile (read from the same selection) agree. None at
    /// LOAD, which expands with the type the row names.
    pub document_type: Option<&'a str>,
    /// items.id=897: the mode profile of the run's launch answer. Applied through
    /// tighten() after the Focus-ceiling clamp, so it can only narrow. None at LOAD.
    pub envelope_override: Option<&'a EnvelopeOverride>,
}

/// items.id=897: what a run-start re-expansion needs from LOAD, kept on the
/// FocusDefinition because an expanded step remembers nothing of its row
/// (block_stable_id and block_customization are None on purpose).
#[derive(Debug, Clone, PartialEq)]
pub struct ModelOrigin {
    /// The composition row's action_id (the step it expanded into).
    pub action_id: String,
    /// The block's stable id (cb-12, cb-13 or cb-14).
    pub stable_id: String,
    pub parsed: ParsedModelBlock,
}

/// One model block in the registry.
pub struct ModelBlockSpec {
    pub stable_id: &'static str,
    pub display_name: &'static str,
    /// LOAD: parse and validate the customization JSON. The bool is
    /// EXTERNAL_MODEL_BLOCKS_ALLOWED.
    pub parse: fn(&Value, bool) -> Result<ParsedModelBlock, String>,
    /// PURE: build the ordinary step.
    pub expand: fn(&ParsedModelBlock, &ExpandInput) -> Result<ActionDefinition, String>,
    /// A valid customization, for the admission test.
    pub example: fn() -> Value,
}

// ---------------------------------------------------------------------------
// The registry of model blocks (consulted by block_dispatch::registered_block)
// ---------------------------------------------------------------------------

pub static CB12_DRAFT: ModelBlockSpec = ModelBlockSpec {
    stable_id: "cb-12",
    display_name: "Draft document",
    parse: draft::parse,
    expand: draft::expand,
    example: draft::example,
};

pub static CB13_VOICE: ModelBlockSpec = ModelBlockSpec {
    stable_id: "cb-13",
    display_name: "Voice (apply or refine)",
    parse: voice::parse,
    expand: voice::expand,
    example: voice::example,
};

pub static CB14_REVISE: ModelBlockSpec = ModelBlockSpec {
    stable_id: "cb-14",
    display_name: "Revise document",
    parse: revise::parse,
    expand: revise::expand,
    example: revise::example,
};

/// items.id=903 (decisions.id=963 ruling 4): pc-03, elicit, one round.
pub static PC03_ELICIT: ModelBlockSpec = ModelBlockSpec {
    stable_id: "pc-03",
    display_name: "Elicit a brief",
    parse: elicit::parse,
    expand: elicit::expand,
    example: elicit::example,
};

/// Every model block's stable id.
pub const MODEL_STABLE_IDS: &[&str] = &["cb-12", "cb-13", "cb-14", "pc-03"];

pub fn model_block_spec(stable_id: &str) -> Option<&'static ModelBlockSpec> {
    match stable_id {
        "cb-12" => Some(&CB12_DRAFT),
        "cb-13" => Some(&CB13_VOICE),
        "cb-14" => Some(&CB14_REVISE),
        "pc-03" => Some(&PC03_ELICIT),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Shared expansion
// ---------------------------------------------------------------------------

/// The destinations a tier can reach.
fn reachable(tier: ExternalAccess) -> Vec<Destination> {
    let mut out = vec![Destination::Local];
    if tier >= ExternalAccess::AnonymousPreferred {
        out.push(Destination::HostedApi);
    }
    if tier >= ExternalAccess::AnonymousRequired {
        out.push(Destination::ManualHandoff);
    }
    out
}

/// What a block's skeleton hands to finish().
pub(crate) struct Built {
    pub step_type: StepType,
    pub template: String,
    pub params: StepParams,
    pub length: WordRange,
    /// items.id=903 (constraint 1, decisions.id=963 ruling 1): the variables the
    /// block cannot run without (cb-12 brief_var, cb-13 apply input_var, cb-14
    /// draft_var and change_var). finish() turns each that is an Optional bound
    /// name into a `present` clause on the expanded step (required_input_clauses).
    pub required: Vec<String>,
}

/// items.id=903 (constraint 1, decisions.id=963 ruling 1): the guard clauses a
/// model block emits for itself. An Optional bound name (an artifact such as
/// content_brief, a hand-off name, run_change) is ABSENT until something writes
/// it, and an absent name renders as nothing, so a draft from an absent brief
/// would run on an empty input. Each required variable that is an Optional bound
/// name gets a `present` clause, in the order given, without duplicates. The loader
/// (and the run-start re-expansion, which keeps the merged list) puts these AHEAD of
/// the row's own clauses (merge_block_clauses), so no Focus row can drop them. A
/// variable that is an earlier row's output, or an Always bound name, gets none.
pub fn required_input_clauses(required: &[String]) -> Vec<Guard> {
    let mut out: Vec<Guard> = Vec::new();
    for var in required {
        let optional = crate::conductor::bound_spec::spec(var)
            .is_some_and(|s| s.presence == crate::conductor::bound_spec::Presence::Optional);
        if optional && !out.iter().any(|g| &g.var == var) {
            out.push(Guard {
                var: var.clone(),
                condition: Condition::Present,
            });
        }
    }
    out
}

/// items.id=903: the clause list of a model-block row: the block's own clauses
/// first, then the row's. A block clause the row already states (as its single
/// guard or in its list) is not repeated, so a row that writes the guard itself
/// still loads. The whole set is then capped by guard::check_clause_total exactly
/// as a row's own list is, so a block clause counts toward MAX_GUARD_CLAUSES.
pub fn merge_block_clauses(
    block: &[Guard],
    row_single: Option<&Guard>,
    row_clauses: Vec<Guard>,
) -> Result<Vec<Guard>, crate::conductor::guard::GuardError> {
    let mut merged: Vec<Guard> = block
        .iter()
        .filter(|g| row_single != Some(*g) && !row_clauses.contains(g))
        .cloned()
        .collect();
    merged.extend(row_clauses);
    crate::conductor::guard::check_clause_total(row_single, &merged)?;
    Ok(merged)
}

/// A temperature or top_p as the JSON number a user would have typed, not an
/// f32 widened to f64 (0.2 stays 0.2).
fn json_number(x: f32) -> Value {
    let rounded = (f64::from(x) * 10_000.0).round() / 10_000.0;
    serde_json::json!(rounded)
}

/// The expansion every model block shares: check the template against the
/// declared inputs and the hosted rule, clamp the range to the Focus ceiling
/// through tighten(), and build the ordinary step.
pub(crate) fn finish(
    stable_id: &str,
    parsed: &ParsedModelBlock,
    input: &ExpandInput,
    built: Built,
) -> Result<ActionDefinition, String> {
    let common = &parsed.common;

    // (1) The template names exactly the declared inputs.
    let named: BTreeSet<String> = find_tokens(&built.template).into_iter().collect();
    let declared: BTreeSet<String> = common.declared_inputs.iter().cloned().collect();
    if let Some(n) = named.difference(&declared).next() {
        return Err(format!(
            "the generated template names '{{{n}}}' but declared_inputs does not list it"
        ));
    }
    if let Some(n) = declared.difference(&named).next() {
        return Err(format!(
            "declared input '{n}' is never used by the generated template (declared_inputs \
             must be exactly the names the block uses)"
        ));
    }

    // (2) The hosted rule.
    if common.range.destinations.contains(&Destination::HostedApi) {
        for banned in ["user_input", "persona_context"] {
            if named.contains(banned) {
                return Err(format!(
                    "a model block that allows hosted_api names neither {{user_input}} nor \
                     {{persona_context}}, and this one names {{{banned}}}"
                ));
            }
        }
    }

    // (3) Clamp the range to the Focus ceiling. An override can only narrow, so a
    // Focus that reaches less than the block allows gets a narrower step instead
    // of failing AUTHORIZE (which rejects a step above its ceiling).
    let clamp = EnvelopeOverride {
        highest_tier: Some(input.focus_ceiling),
        destinations: Some(reachable(input.focus_ceiling)),
        ..Default::default()
    };
    let mut env = tighten(&common.envelope(), &clamp)
        .map_err(|e| format!("the range cannot be clamped to the Focus ceiling: {e}"))?;
    // (3b) items.id=897: the mode profile of the run's launch answer, again only
    // through tighten(). A profile may not remove a declared input: the generated
    // template already names it, so the step would name an input it no longer
    // declares. Refused here, so the LOAD trial expansion reports it.
    if let Some(over) = input.envelope_override {
        env = tighten(&env, over)
            .map_err(|e| format!("the mode profile cannot be applied to this block: {e}"))?;
        if env.declared_inputs.len() != common.declared_inputs.len() {
            return Err(
                "a mode profile may not remove a declared input from a block whose generated \
                 template names it"
                    .to_owned(),
            );
        }
    }
    let range = config::ModelRange {
        quality_floor: env.quality_floor,
        highest_tier: env.highest_tier,
        destinations: env.destinations,
    };
    config::validate_range(&range, input.external_allowed)?;

    let tier = range.highest_tier;
    let mut options_override: HashMap<String, Value> = HashMap::new();
    options_override.insert(
        "temperature".to_owned(),
        json_number(built.params.temperature),
    );
    if let Some(tp) = built.params.top_p {
        options_override.insert("top_p".to_owned(), json_number(tp));
    }
    options_override.insert(
        "num_predict".to_owned(),
        serde_json::json!(built.length.num_predict),
    );

    Ok(ActionDefinition {
        action_id: input.action_id.to_owned(),
        display_name: common.display_name.clone(),
        guide_id: MODEL_BLOCK_GUIDE_ID.to_owned(),
        task_type: common.need.task_type.as_str().to_owned(),
        routing_tier: tier,
        step_type: built.step_type,
        output_var: Some(common.output_var.clone()),
        prompt_template: built.template,
        field_requirements: HashMap::new(),
        options_override,
        // Same derivation build_action_definition uses: never Some at unrestricted.
        external_access_override: if tier == ExternalAccess::Unrestricted {
            None
        } else {
            Some(tier)
        },
        revisitable: built.step_type.default_revisitable(),
        // A model block is not a pure block: see the module comment.
        requires_user_handoff: false,
        block_stable_id: None,
        block_customization: None,
        schedule_trigger: None,
        guard: None,
        // items.id=903: the block's own required-input clauses. The loader merges
        // the row's clauses after them (merge_block_clauses).
        and_guards: required_input_clauses(&built.required),
        declared_inputs: common.declared_inputs.clone(),
        store_op: None,
        requirement: Some(ModelRequirement {
            block_stable_id: stable_id.to_owned(),
            need: common.need.clone(),
            range,
            class_preference: common.class_preference,
        }),
    })
}

/// Resolve the document type a row names, with the settings in force.
pub(crate) fn resolve_type<'a>(
    document_type: &str,
    input: &'a ExpandInput,
    reg: &'a document_type::Registry,
) -> Result<(&'a document_type::DocumentType, Settings), String> {
    // items.id=897: the run's selected type replaces the one the row names.
    let document_type = input.document_type.unwrap_or(document_type);
    let t = reg
        .document_type(document_type)
        .ok_or_else(|| format!("unknown document_type '{document_type}'"))?;
    let settings = input
        .settings
        .cloned()
        .unwrap_or_else(|| t.settings.clone());
    reg.scales
        .check_settings(&settings)
        .map_err(|e| format!("document type '{document_type}': {e}"))?;
    Ok((t, settings))
}

/// Parse the two-object customization shape `{"common": {...}, "spec": {...}}`,
/// naming the unknown top-level key in the error. serde cannot combine `flatten`
/// with `deny_unknown_fields`, hence the two nested objects.
pub(crate) fn parse_row<S: serde::de::DeserializeOwned>(
    block: &str,
    value: &Value,
    external_allowed: bool,
) -> Result<(CommonConfig, S), String> {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Row<S> {
        common: CommonConfig,
        spec: S,
    }
    let row: Row<S> = serde_json::from_value(value.clone()).map_err(|e| format!("{block}: {e}"))?;
    row.common
        .validate(external_allowed)
        .map_err(|e| format!("{block}: {e}"))?;
    Ok((row.common, row.spec))
}

/// A variable name a spec supplies (brief_var, sender_var, ...): a lower-case
/// name.
pub(crate) fn check_var(block: &str, what: &str, name: &str) -> Result<(), String> {
    if !config::is_slug(name) {
        return Err(format!(
            "{block}: {what} '{name}' must be a lower-case name"
        ));
    }
    Ok(())
}
