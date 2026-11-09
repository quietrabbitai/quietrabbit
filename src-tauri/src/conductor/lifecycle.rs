// src-tauri/src/conductor/lifecycle.rs
//
// FocusRun — Conductor execution engine's seven-phase lifecycle.
// FocusDefinition — parsed .focus YAML file representation.
// RunResult — structured outcome from a completed or interrupted run.
// demote_interrupted_runs() — standalone async fn for startup recovery.
//
// Phase 1 LOAD:       parse .focus file via serde-saphyr; validate steps
// Phase 2 AUTHORIZE:  focus_settings tier check; create focus_run record (initializing)
// Phase 3 INITIALIZE: build + seal PersonalTrack; assemble TaskTrack + SharedStateTrack;
//                     construct PrivacyGateway; assemble persona context (async);
//                     promote focus_run to running
// Phase 4 EXECUTE:    step loop — cloud_frontier steps are terminal boundaries;
//                     current_step: usize is an explicit field (not an implicit counter)
// Phase 5 OUTPUT:     save output to outputs.db; purge snapshots; write run_history
// Phase 6 FEEDBACK:   out of scope for this module (async paste-back)
// Phase 7 CLEANUP:    drop tracks; enforce snapshot retention; update final status
//
// Architectural mandates (D6-347):
//   - FocusRun is a single Tokio actor owning all three tracks (D6-342)
//   - execute_full() is sequential 7-phase method, NOT a message-driven state machine
//   - current_step: usize is an explicit field for resume correctness (not implicit counter)
//   - emit() at each step boundary via AppHandle for run_status_update push events (D6-345)
//   - Cancellation/consent pause points via explicit checks within the step loop
//
// Track ownership (D6-342):
//   FocusRun owns PersonalTrack, TaskTrack, SharedStateTrack as Option<T> fields.
//   execute_step() borrows distinct fields simultaneously — the borrow checker allows
//   this via split field borrows within a single &mut self call. No Arc<Mutex<>> needed.
//
// Rust deviation from Python oracle — StepContext:
//   Python StepContext holds all tracks and the privacy_gateway by reference.
//   PersonalTrack does not implement Clone; FocusRun uses split field borrows instead.
//   StepContext holds step metadata only (step, tier values, run IDs, user input).
//   PersonalTrack, TaskTrack, SharedStateTrack, FailureHandler, PrivacyGateway, and
//   ConductorScheduler are passed as separate parameters to StepExecutor::execute().
//   See executor.rs for the full function signature.
//
// Privacy gateway:
//   FocusRun<L: DisclosureLogger = SqliteDisclosureLogger> is generic over
//   its disclosure logger (items.id=173, "Layer 8"). Production
//   construction (initialize(), Phase 3) wires SqliteDisclosureLogger,
//   which persists every disclosure-log entry to the disclosure_log table
//   in personal.db. Tests can instantiate FocusRun<TestLogger> or
//   FocusRun<NoopLogger> explicitly to inject the DisclosureLogger they
//   need. Default type param means every pre-existing FocusRun::new(...)
//   call site (commands/execution.rs, this file's own test helpers) keeps
//   compiling unchanged and now gets the concrete logger for free.
//
// serde-saphyr (items.id=733; was serde_yaml) replaces Python hand-parser:
//   FocusDefinition populated via intermediate RawFocusFile deserialization.
//   brief: block deserialized but discarded — never wired to runtime behavior (D6-343).
//
// Floor invariant violations: explicit Err() returns, NOT panic (D6-348).
//   Invariant checks live in executor.rs::execute_once(). Lifecycle computes and
//   passes tier values; executor enforces invariants.
//
// Commit note: lifecycle.rs and executor.rs are tightly coupled (lifecycle imports
//   StepContext + StepExecutor from executor). Both files are committed in a single
//   two-file commit. cargo build is verified after both are written.
//
// Rename chain (CLAUDE.md):
//   PathDefinition -> FocusDefinition | PathRun -> FocusRun
//   space_id -> life_id -> persona_id (D6-298)
//   life_context -> persona_context (D6-298/D6-323, token name and internal field — rename complete)
//   focus_runs, focus_run_snapshots (SQL table identifiers)

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use chrono::{DateTime, Duration, Utc};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::ConnectOptions;
use sqlx::Row;
use sqlx::SqliteConnection;
use tauri::Manager;
use thiserror::Error;
use uuid::Uuid;

use crate::conductor::concurrency::ConductorScheduler;
use crate::conductor::executor::{is_user_stop, StepContext, StepExecutor};
use crate::conductor::failure::{
    ConductorError, FailureAction, FailureHandler, FailureResult, FailureSeverity,
};
use crate::conductor::guard;
use crate::conductor::memory_broker::MemoryBroker;
use crate::conductor::privacy::output_scan::{scan_output, ScanIntensity};
use crate::conductor::privacy::types::sensitivity_severity;
use crate::conductor::privacy::{logger::DisclosureLoggerForRun, PrivacyGateway};
use crate::conductor::reentry::{self, ReentryError, ReentryPlan};
use crate::conductor::store_op;
use crate::conductor::tokens::{
    validate_step, ActionDefinition, ExternalAccess, FieldRequirement, StepType,
};
use crate::conductor::types::{
    PersonalContextManifest, PersonalTrack, PromotedContentEntry, SharedStateTrack, TaskStep,
    TaskTrack,
};
use crate::persistence::disclosure_log_store::SqliteDisclosureLogger;
use crate::providers::utils::{connect_options_encrypted, db_path_outputs, now};

// ---------------------------------------------------------------------------
// LifecycleError
// ---------------------------------------------------------------------------

/// Covers all failure modes across the seven lifecycle phases.
///
/// TaxonomyIntegrity and DatabaseMigration are F_SYSTEM variants — caught in
/// execute_full() and converted to a structured FailureResult rather than
/// propagating as Err(). All other variants propagate to the caller.
/// Python oracle: multiple exception types raised by phase methods.
#[derive(Debug, Error)]
pub enum LifecycleError {
    /// The account's key epoch changed since this session unlocked
    /// (auth::epoch_guard, items.id=629).
    #[error("{0}")]
    KeyEpoch(#[from] crate::auth::epoch_guard::KeyEpochError),
    /// A database key could not be derived from the data key
    /// (auth::subkeys, items.id=630).
    #[error("{0}")]
    Subkey(#[from] crate::auth::subkeys::SubkeyError),
    // Phase 1 — LOAD
    #[error("Focus file not found: {0}.focus")]
    FocusNotFound(String),
    /// Plain-text parser message (no source snippet, so an authored prompt is
    /// never echoed back in an error).
    #[error("YAML parse error: {0}")]
    YamlParse(String),
    #[error("Focus validation failed: {0}")]
    ValidationFailed(String),

    // Phase 2 — AUTHORIZE
    #[error("Session expired — decryption key required")]
    NoKey,
    #[error("Tier ceiling violation: {0}")]
    TierViolation(String),
    #[error("Persona not found: {0}")]
    PersonaNotFound(String),
    #[error("Focus settings not found: {0}")]
    FocusSettingsNotFound(String),

    // Rehydrate (items.id=574 shared foundation / items.id=245)
    #[error("Focus run not found: {0}")]
    RunNotFound(String),

    // reenter_step() (items.id=574 follow-up)
    #[error("Reentry: {0}")]
    Reentry(#[from] ReentryError),

    // F_SYSTEM — caught in execute_full() for FailureResult mapping
    #[error("Taxonomy integrity: {0}")]
    TaxonomyIntegrity(String),
    #[error("Database migration: {0}")]
    DatabaseMigration(String),

    // Cross-Persona Data Provenance (decisions.id=546, items.id=27).
    // Raised when an entity_facts row has source_persona_id != current
    // persona_id but cross_persona_export = false. Per decisions.id=546:
    // "flagged as a system integrity error, not a user-facing privacy
    // warning, since correct fact-model behavior should make this case
    // unreachable." Distinct from PrivacyGateBlocked (ConductorError) —
    // that variant is for normal Gate1-4 policy blocks; this is a fact-model
    // corruption signal, fires at Phase 3 INITIALIZE before any step runs.
    #[error("Provenance integrity violation: {0}")]
    ProvenanceIntegrityViolation(String),

    // Infrastructure
    #[error("Database: {0}")]
    Database(#[from] sqlx::Error),
    #[error("Personal store: {0}")]
    PersonalStore(String),
    #[error("Output store: {0}")]
    OutputStore(String),
    #[error("Privacy scan: {0}")]
    PrivacyScan(String),
    #[error("Persona store: {0}")]
    PersonaStore(String),
    #[error("Focus settings store: {0}")]
    FocusSettingsStore(String),
    #[error("Topic store: {0}")]
    TopicStore(String),
    /// items.id=886: the pass could not read the session or case store it needs,
    /// so it did not run (fail closed, decisions.id=939 section 2). The message
    /// names the variable, never its value.
    #[error("Session store: {0}")]
    StoreSeed(#[from] crate::conductor::seeding::SeedError),
    /// items.id=887: a store writer row failed, so the pass stops and nothing
    /// after it runs (fail closed). A one-time chat's refusal is not this: it
    /// is an Ok outcome that writes nothing.
    #[error("Session store write: {0}")]
    StoreWrite(#[from] crate::conductor::store_writer::StoreWriteError),
    #[error("JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("IO: {0}")]
    Io(#[from] std::io::Error),
}

// ---------------------------------------------------------------------------
// Internal serde-saphyr deserialization structs (private)
// ---------------------------------------------------------------------------
// These map 1:1 to the .focus YAML file structure. parse_focus_definition()
// converts raw -> domain types. brief: block is parsed but discarded (D6-343).

#[derive(Deserialize)]
struct RawFocusFile {
    id: Option<String>,
    display_name: Option<String>,
    description: Option<String>,
    version: Option<serde_json::Value>, // YAML may parse 1.0 as a Number; stringify
    max_routing_tier: Option<u8>,
    output_types: Option<Vec<String>>, // conductor-brief (preferred)
    output_type: Option<String>,       // legacy single value — fallback only
    guides: Option<Vec<String>>,       // focus-level; first entry = step default
    suggest_in_focuses: Option<Vec<String>>,
    multi_source_validation: Option<bool>,
    steps: Option<IndexMap<String, RawStep>>, // IndexMap preserves YAML dict order
    // D6-343: accepted in any shape for forward compat, not wired in Release 1.
    // IgnoredAny reads and drops it, so a brief can never fail a Focus load.
    #[allow(dead_code)]
    brief: Option<serde::de::IgnoredAny>,
    // decisions.id=513 (D6-471), items.id=175. No .focus file in this repo
    // declares this section yet — Option so every existing file keeps
    // parsing unchanged. Only generic_title_template is built now; type
    // policy (status/age/always-visible conditions) is a separate, larger
    // piece of decisions.id=513 scoped out until the first Focus that
    // declares one is built (see conductor/visibility.rs module header).
    display_config: Option<RawDisplayConfig>,
}

#[derive(Deserialize)]
struct RawDisplayConfig {
    // decisions.id=513: "Focus declares generic title template in .focus
    // file display_config section... Template must handle full range of
    // object states — not just the open-ticket case." Substitution syntax
    // and consuming call sites (get_active_board and other Ambient
    // surfaces) are a separate, later wiring task — this struct only
    // carries the declared string through parsing.
    generic_title_template: Option<String>,
    // decisions.id=712: Active Board high-priority section trigger.
    // Declared once per Focus type at build time, not per-instance.
    // No .focus file in this repo declares this yet (Travel/Habit are
    // not built — see items.id=236 correction) -- Option so every
    // existing file keeps parsing unchanged.
    high_priority_trigger: Option<RawHighPriorityTrigger>,
}

#[derive(Deserialize)]
struct RawHighPriorityTrigger {
    // Key into a Topic's extra_metadata JSON where the anchor RFC3339
    // timestamp lives for this Focus type.
    anchor_field: String,
    // Signed time offset relative to the anchor, e.g. "-4h", "+0", "-1d".
    offset: String,
}

#[derive(Deserialize)]
struct RawStep {
    display_name: Option<String>,
    guide_id: Option<String>, // overrides focus-level default if present
    task_type: Option<String>,
    routing_tier: Option<u8>,
    // items.id=528 Phase 2: authored directly by the step, decoupled from
    // routing_tier. Absent -> falls back to routing_tier == 3 (see
    // parse_focus_definition's own derivation) for back-compat with every
    // .focus file authored before this field existed.
    requires_user_handoff: Option<bool>,
    step_type: Option<String>,
    output_var: Option<String>,
    prompt_template: Option<String>,
    field_requirements: Option<Vec<RawFieldRequirement>>,
    options_override: Option<serde_json::Value>, // YAML map -> HashMap<String, json::Value>
    // items.id=879: same field name in the YAML step and the database
    // customization JSON. Absent = the step declares nothing (default none).
    declared_inputs: Option<Vec<String>>,
}

#[derive(Deserialize)]
struct RawFieldRequirement {
    name: String,
    scope: String,
}

// ---------------------------------------------------------------------------
// parse_focus_definition
// ---------------------------------------------------------------------------

/// Parse a decisions.id=712 signed time-offset string, e.g. "-4h", "+0",
/// "-1d". Grammar: optional sign (default +), then either a bare `0`
/// (unit optional -- unambiguous, 0h == 0d) or `<int><unit>` where unit is
/// `h` (hours) or `d` (days). Anything else is a parse error.
fn parse_offset(raw: &str) -> Result<Duration, String> {
    let s = raw.trim();
    let (sign, rest): (i64, &str) = match s.as_bytes().first() {
        Some(b'+') => (1, &s[1..]),
        Some(b'-') => (-1, &s[1..]),
        _ => (1, s),
    };
    if rest == "0" {
        return Ok(Duration::zero());
    }
    if rest.is_empty() {
        return Err(format!("invalid offset: {raw:?}"));
    }
    let unit_pos = rest.len() - 1;
    let (digits, unit) = rest.split_at(unit_pos);
    let n: i64 = digits
        .parse()
        .map_err(|_| format!("invalid offset: {raw:?}"))?;
    let signed_n = sign * n;
    match unit {
        "h" => Ok(Duration::hours(signed_n)),
        "d" => Ok(Duration::days(signed_n)),
        _ => Err(format!("invalid offset unit in {raw:?} (expected h or d)")),
    }
}

/// Converts a .focus YAML file's raw routing_tier/max_routing_tier number
/// (1, 2, or 3) to ExternalAccess. items.id=529: the only remaining
/// numeric-literal -> ExternalAccess conversion in the codebase, since
/// storage is string-native now (ExternalAccess::from_str()) -- this one
/// exists solely because the .focus YAML authoring surface itself still
/// uses the raw number (no YAML field-name/format change in this item's
/// scope; AnonymousPreferred has no YAML-authorable form yet). Panics on an
/// out-of-range value exactly as the retired ExternalAccess::from_legacy_tier()
/// did -- .focus files are shipped/curated content, not user input.
fn external_access_from_routing_tier_yaml(raw: u8) -> ExternalAccess {
    match raw {
        1 => ExternalAccess::LocalOnly,
        2 => ExternalAccess::AnonymousRequired,
        3 => ExternalAccess::Unrestricted,
        other => unreachable!(
            "routing_tier/max_routing_tier value {other} is not 1, 2, or 3 -- \
             .focus YAML authoring does not yet support AnonymousPreferred \
             (items.id=529 scope item 4, unresolved)"
        ),
    }
}

/// Bridges an ExternalAccess value down to the numeric execution_tier axis
/// (StepContext.execution_tier: u8 and its downstream consumers --
/// executor.rs, gate1.rs/gate2.rs, abstraction.rs, MemoryBroker::
/// assemble_context(), scan_output()). items.id=529 deliberately left that
/// axis's own >1/>=2/==1/==2 magnitude checks as raw-integer comparisons
/// (items.id=528 Job 2 #2 territory, sequenced after this item) -- this is
/// NOT a legacy-storage round-trip (that concept is retired entirely; see
/// ExternalAccess's own doc comment), just the numeric side of a still-
/// numeric axis that this item's Axis-1 ceiling calc must keep feeding.
///
/// AnonymousPreferred -> 3, matching Unrestricted (items.id=537, decisions.id=820):
/// fail closed. AnonymousPreferred's actual provider eligibility (any
/// provider, anonymous or full-account) matches Unrestricted's exactly
/// (tokens.rs's ExternalAccess doc comment) -- this axis abstracts and
/// gates on reach, not on which provider a run happens to prefer, so a run
/// that may reach a full-account provider must be treated at the same
/// tier as one that always does. This DOES change behavior at the three
/// call sites that distinguish 2 from 3 (items.id=537 consumer audit):
/// privacy/abstraction.rs AbstractionLevel::from_tier (AnonymousPreferred
/// now abstracts at Level3, not Level2), the memory-store tier-eligibility
/// tables (domain_context_store.rs, plan_state_store.rs), and gate3's
/// cross-tier promotion check in executor.rs (next_tier > execution_tier
/// now fires AnonymousRequired -> AnonymousPreferred, and no longer fires
/// AnonymousPreferred -> Unrestricted, since reach is equal and nothing
/// widens). See items.id=537's description for the full audit.
fn execution_tier_ordinal(access: ExternalAccess) -> u8 {
    match access {
        ExternalAccess::LocalOnly => 1,
        ExternalAccess::AnonymousRequired => 2,
        ExternalAccess::AnonymousPreferred => 3,
        ExternalAccess::Unrestricted => 3,
    }
}

/// Already-source-normalized per-step fields, shared between the YAML path
/// (parse_focus_definition(), below) and the DB-composition path
/// (load_focus_definition_from_db(), items.id=496). field_requirements and
/// options_override arrive pre-converted (Vec<{name,scope}> -> HashMap,
/// source Value -> HashMap<String, serde_json::Value>) because that
/// conversion is genuinely source-format-specific (a parsed YAML value vs
/// plain JSON); everything build_action_definition() does with these fields
/// is not.
struct NormalizedRawStep {
    display_name: Option<String>,
    guide_id: Option<String>,
    task_type: Option<String>,
    routing_tier: Option<u8>,
    requires_user_handoff: Option<bool>,
    step_type: Option<String>,
    output_var: Option<String>,
    prompt_template: Option<String>,
    field_requirements: HashMap<String, FieldRequirement>,
    options_override: HashMap<String, serde_json::Value>,
    /// items.id=879: names an external step's template may use, beyond its own
    /// field_requirements (conductor::declared_inputs). None = declares nothing.
    declared_inputs: Option<Vec<String>>,
}

/// Shared ActionDefinition derivation (items.id=496): the routing_tier ->
/// requires_user_handoff / external_access_override / revisitable math
/// (items.id=439/528/574) is identical regardless of whether a step came
/// from a .focus YAML file or a focus_block_compositions/
/// focus_block_customization row pair -- this is the literal code both
/// paths run, not a re-implementation, which is what "produce the
/// equivalent of today's fixed ActionDefinition sequence" concretely means.
/// block_stable_id/block_customization/schedule_trigger are always None
/// here -- the DB path sets them itself afterward; a YAML-authored step
/// never has them (see load_focus_definition_from_db()).
fn build_action_definition(
    action_id: String,
    default_guide_id: &str,
    raw: NormalizedRawStep,
) -> ActionDefinition {
    let step_type = raw
        .step_type
        .as_deref()
        .and_then(|s| s.parse::<StepType>().ok())
        .unwrap_or_default();

    // items.id=439 (Part 6c/6d), corrected by items.id=528 Phase 2:
    // requires_user_handoff and external_access_override are independently
    // derived, not coupled through each other -- see parse_focus_definition's
    // historical comment (git blame) for the full reasoning; unchanged by
    // this extraction.
    let raw_routing_tier = raw.routing_tier.unwrap_or(1);
    let requires_user_handoff = raw.requires_user_handoff.unwrap_or(raw_routing_tier == 3);
    let routing_tier = external_access_from_routing_tier_yaml(raw_routing_tier);
    let external_access_override = if routing_tier == ExternalAccess::Unrestricted {
        None
    } else {
        Some(routing_tier)
    };
    let revisitable = step_type.default_revisitable();

    ActionDefinition {
        display_name: raw.display_name.unwrap_or_else(|| action_id.clone()),
        guide_id: raw.guide_id.unwrap_or_else(|| default_guide_id.to_owned()),
        task_type: raw.task_type.unwrap_or_else(|| "general".to_owned()),
        routing_tier,
        step_type,
        output_var: raw.output_var,
        prompt_template: raw.prompt_template.unwrap_or_default(),
        field_requirements: raw.field_requirements,
        options_override: raw.options_override,
        external_access_override,
        requires_user_handoff,
        revisitable,
        block_stable_id: None,
        block_customization: None,
        schedule_trigger: None,
        guard: None,
        declared_inputs: raw.declared_inputs.unwrap_or_default(),
        store_op: None,
        and_guards: Vec::new(),
        action_id,
    }
}

/// Convert RawFocusFile -> FocusDefinition.
/// Replaces Python's hand-written _parse_focus_definition() with serde-saphyr.
/// Applies the same shims: guide_id inheritance, output_types->output_type,
/// field_requirements list -> HashMap.
fn parse_focus_definition(raw: RawFocusFile) -> Result<FocusDefinition, LifecycleError> {
    let focus_id = raw.id.unwrap_or_default();

    // Focus-level guides list: first entry is the step default (COMPATIBILITY shim).
    // conductor-brief declares guides at focus level; ActionDefinition still requires
    // a guide_id per step. When multi-guide Focuses exist, per-step guide_id required.
    let default_guide_id = raw
        .guides
        .as_ref()
        .and_then(|v| v.first())
        .cloned()
        .unwrap_or_else(|| "quick-ask-guide".to_owned());

    // output_types (plural, conductor-brief preferred) -> single output_type.
    // Falls back to legacy output_type key, then "general".
    let output_type = raw
        .output_types
        .as_ref()
        .and_then(|v| v.first())
        .cloned()
        .or(raw.output_type)
        .unwrap_or_else(|| "general".to_owned());

    // Version: YAML may parse "1.0" as float Number. str(value) in Python.
    let version = match raw.version {
        Some(serde_json::Value::String(s)) => s,
        Some(serde_json::Value::Number(n)) => n.to_string(),
        Some(_) | None => "1.0".to_owned(),
    };

    let suggest_in_focuses = raw.suggest_in_focuses.unwrap_or_default();

    // decisions.id=513: generic_title_template, defaulting to a platform-
    // wide generic string when the Focus declares no display_config section
    // at all (every existing .focus file in this repo, as of items.id=175 —
    // this default keeps them all parsing unchanged).
    //
    // decisions.id=712: high_priority_trigger, parsed alongside it since
    // both live under the same display_config section. A malformed offset
    // string fails the whole Focus load, matching step-validation behavior.
    let (generic_title_template, high_priority_trigger) = match raw.display_config {
        Some(dc) => {
            let title = dc
                .generic_title_template
                .unwrap_or_else(|| "Hidden item".to_owned());
            let trigger = match dc.high_priority_trigger {
                Some(t) => {
                    let offset = parse_offset(&t.offset).map_err(|e| {
                        LifecycleError::ValidationFailed(format!(
                            "display_config.high_priority_trigger.offset: {e}"
                        ))
                    })?;
                    Some(HighPriorityTrigger {
                        anchor_field: t.anchor_field,
                        offset,
                    })
                }
                None => None,
            };
            (title, trigger)
        }
        None => ("Hidden item".to_owned(), None),
    };

    let mut steps: Vec<ActionDefinition> = Vec::new();
    if let Some(raw_steps) = raw.steps {
        for (action_id_key, raw_step) in raw_steps {
            // field_requirements: list of {name, scope} -> HashMap<String, FieldRequirement>
            let field_requirements = raw_step
                .field_requirements
                .unwrap_or_default()
                .into_iter()
                .filter_map(|fr| {
                    fr.scope
                        .parse::<FieldRequirement>()
                        .ok()
                        .map(|req| (fr.name, req))
                })
                .collect::<HashMap<_, _>>();

            // options_override: YAML map -> HashMap<String, serde_json::Value>.
            // The field is read straight into a serde_json::Value.
            let options_override = raw_step
                .options_override
                .and_then(|jv| {
                    if let serde_json::Value::Object(map) = jv {
                        Some(map.into_iter().collect::<HashMap<_, _>>())
                    } else {
                        None
                    }
                })
                .unwrap_or_default();

            // items.id=439 (Part 6c/6d), corrected by items.id=528 Phase 2:
            // requires_user_handoff authored directly via the step's own
            // .focus YAML field (added this pass) when present; falls back
            // to raw_routing_tier == 3 when absent -- byte-identical for
            // every .focus file authored before this field existed (none of
            // the 4 shipped Focuses use routing_tier: 3). See
            // build_action_definition() for the rest of the derivation
            // (items.id=496 extracted it for reuse by the DB-composition
            // path) -- external_access_override deliberately NOT gated on
            // requires_user_handoff (the two conditions used to always
            // coincide; see git blame for the full historical reasoning).
            steps.push(build_action_definition(
                action_id_key.clone(),
                &default_guide_id,
                NormalizedRawStep {
                    display_name: raw_step.display_name,
                    guide_id: raw_step.guide_id,
                    task_type: raw_step.task_type,
                    routing_tier: raw_step.routing_tier,
                    requires_user_handoff: raw_step.requires_user_handoff,
                    step_type: raw_step.step_type,
                    output_var: raw_step.output_var,
                    prompt_template: raw_step.prompt_template,
                    field_requirements,
                    options_override,
                    declared_inputs: raw_step.declared_inputs,
                },
            ));
        }
    }

    let max_routing_tier =
        external_access_from_routing_tier_yaml(raw.max_routing_tier.unwrap_or(1));

    Ok(FocusDefinition {
        display_name: raw.display_name.unwrap_or_else(|| focus_id.clone()),
        description: raw.description.unwrap_or_default(),
        // items.id=439 (Part 6, Jason's three-way-fold resolution): the
        // Focus-author structural ceiling, folded into FocusRun's
        // _focus_external_access alongside focus_settings.max_permitted_tier
        // -- see authorize() below. items.id=529: this field used to be a
        // raw u8 with a separate max_external_access: ExternalAccess field
        // derived from it at parse time (a typed shadow, needed only
        // because this field itself had to stay numeric for the
        // execution_tier calc); now that the field is ExternalAccess
        // natively, that shadow field was a pure duplicate and was removed.
        max_routing_tier,
        multi_source_validation: raw.multi_source_validation.unwrap_or(false),
        focus_id,
        version,
        steps,
        output_type,
        suggest_in_focuses,
        generic_title_template,
        high_priority_trigger,
    })
}

/// items.id=496: DB-composition path's per-step JSON shape -- mirrors
/// RawStep field-for-field except options_override, which is JSON-native
/// here (no YAML value conversion needed -- the source is already
/// JSON, unlike a .focus file). Deserialized from
/// focus_block_customization.customization when a composition row's
/// block_stable_id is NULL (see load_focus_definition_from_db(), below).
#[derive(Deserialize, Default)]
struct RawStepFromDb {
    display_name: Option<String>,
    guide_id: Option<String>,
    task_type: Option<String>,
    routing_tier: Option<u8>,
    requires_user_handoff: Option<bool>,
    step_type: Option<String>,
    output_var: Option<String>,
    prompt_template: Option<String>,
    field_requirements: Option<Vec<RawFieldRequirement>>,
    options_override: Option<serde_json::Value>,
    // items.id=879: see RawStep::declared_inputs.
    declared_inputs: Option<Vec<String>>,
}

/// items.id=496: the DB-composition path. Returns Ok(None) when no
/// `focuses` row exists for `focus_id` in shared.db -- the signal
/// load_focus_definition() (below) uses to fall back to the .focus YAML
/// file, not an error. Every YAML-authored Focus has no such row
/// (decisions.id=595 explicitly defers migrating the shipped 5 -- see
/// shared_021.sql's own header comment), so this is a pure no-op for all of
/// them today.
///
/// A composition row with block_stable_id NULL runs its customization JSON
/// through the exact same build_action_definition() helper
/// parse_focus_definition() uses for a YAML step -- this IS what "produce
/// the equivalent of today's fixed ActionDefinition sequence" means: literal
/// code reuse, not a re-implementation (items.id=496 judgment calls 2/5).
/// A composition row with block_stable_id Some(id) must pass three LOAD-time
/// checks, each with its own error (items.id=129):
///   1. mirror membership: `id` is in shared.db's building_blocks mirror as
///      an inline_composable block (items.id=496 judgment call 7);
///   2. handler existence: conductor::block_dispatch::block_handler(id) is
///      Some -- a cataloged block with no compiled-in handler fails here, not
///      mid-run;
///   3. customization: the row's JSON parses into the handler's typed,
///      deny_unknown_fields struct (BlockHandler::validate).
///
/// The validated JSON is then carried on the step for execute_step(), which
/// dispatches through the same block_handler() registry.
async fn load_focus_definition_from_db(
    pool: &sqlx::SqlitePool,
    focus_id: &str,
) -> Result<Option<FocusDefinition>, LifecycleError> {
    use crate::persistence::focus_composition_store as store;

    let Some(focus_row) = store::get_focus(pool, focus_id)
        .await
        .map_err(LifecycleError::Database)?
    else {
        return Ok(None);
    };

    let max_routing_tier = focus_row
        .max_routing_tier
        .parse::<ExternalAccess>()
        .map_err(LifecycleError::ValidationFailed)?;

    let high_priority_trigger = parse_optional_trigger(
        focus_id,
        None,
        focus_row.high_priority_trigger_anchor_field,
        focus_row.high_priority_trigger_offset,
    )?;

    let composition_rows = store::list_compositions(pool, focus_id)
        .await
        .map_err(LifecycleError::Database)?;

    // No `guides` column on `focuses` (items.id=496 scope: a composition
    // row's customization JSON is expected to name guide_id explicitly --
    // there is no per-step YAML shorthand to preserve here). Falls back to
    // the same global default RawFocusFile.guides == None already uses.
    let default_guide_id = "quick-ask-guide";

    let mut steps = Vec::with_capacity(composition_rows.len());
    for row in composition_rows {
        let schedule_trigger = parse_optional_trigger(
            focus_id,
            Some(&row.action_id),
            row.schedule_trigger_anchor_field,
            row.schedule_trigger_offset,
        )?;

        // items.id=878: an unparseable guard, an unknown operator or a
        // malformed value each fail LOAD here with their own message. Whether
        // the guard's variable exists is checked below, over the whole row list.
        let guard = guard::Guard::from_columns(
            row.guard_var.as_deref(),
            row.guard_op.as_deref(),
            row.guard_value.as_deref(),
        )
        .map_err(|e| {
            LifecycleError::ValidationFailed(format!(
                "Focus '{focus_id}' step '{}': {e}",
                row.action_id
            ))
        })?;

        // items.id=889: the AND-guard clause list (shared_042.sql). Each clause
        // is parsed with the same operators as the single guard, the total over
        // both is capped, and a malformed list fails LOAD with its own message.
        let and_guards = guard::parse_clauses(row.guard_clauses.as_deref())
            .and_then(|clauses| {
                guard::check_clause_total(guard.as_ref(), &clauses)?;
                Ok(clauses)
            })
            .map_err(|e| {
                LifecycleError::ValidationFailed(format!(
                    "Focus '{focus_id}' step '{}': {e}",
                    row.action_id
                ))
            })?;

        // items.id=887: the typed store-writer columns. A malformed one fails
        // LOAD here with its own message, as a malformed guard does.
        let store_writer = store_op::StoreOp::from_columns(
            row.store_op.as_deref(),
            row.store_target.as_deref(),
            row.store_args.as_deref(),
        )
        .map_err(|e| {
            LifecycleError::ValidationFailed(format!(
                "Focus '{focus_id}' step '{}': {e}",
                row.action_id
            ))
        })?;

        // A writer is neither a model step nor a cataloged block: it is built
        // here, guard and all, and the rest of the loop is for the other two.
        if let Some(op) = store_writer {
            let mut step = store_writer_step(
                focus_id,
                &row.action_id,
                row.block_stable_id.is_some(),
                schedule_trigger.is_some(),
                &row.customization,
                default_guide_id,
                op,
            )?;
            step.guard = guard;
            step.and_guards = and_guards;
            steps.push(step);
            continue;
        }

        let mut step = match &row.block_stable_id {
            None => {
                let raw_step: RawStepFromDb =
                    serde_json::from_str(&row.customization).map_err(|e| {
                        LifecycleError::ValidationFailed(format!(
                            "Focus '{focus_id}' step '{}': customization JSON: {e}",
                            row.action_id
                        ))
                    })?;
                let field_requirements = raw_step
                    .field_requirements
                    .unwrap_or_default()
                    .into_iter()
                    .filter_map(|fr| {
                        fr.scope
                            .parse::<FieldRequirement>()
                            .ok()
                            .map(|req| (fr.name, req))
                    })
                    .collect::<HashMap<_, _>>();
                let options_override = raw_step
                    .options_override
                    .and_then(|v| {
                        if let serde_json::Value::Object(map) = v {
                            Some(map.into_iter().collect::<HashMap<_, _>>())
                        } else {
                            None
                        }
                    })
                    .unwrap_or_default();
                build_action_definition(
                    row.action_id.clone(),
                    default_guide_id,
                    NormalizedRawStep {
                        display_name: raw_step.display_name,
                        guide_id: raw_step.guide_id,
                        task_type: raw_step.task_type,
                        routing_tier: raw_step.routing_tier,
                        requires_user_handoff: raw_step.requires_user_handoff,
                        step_type: raw_step.step_type,
                        output_var: raw_step.output_var,
                        prompt_template: raw_step.prompt_template,
                        field_requirements,
                        options_override,
                        declared_inputs: raw_step.declared_inputs,
                    },
                )
            }
            Some(stable_id) => {
                // Check 1: mirror membership.
                if !store::is_inline_composable_block(pool, stable_id)
                    .await
                    .map_err(LifecycleError::Database)?
                {
                    return Err(LifecycleError::ValidationFailed(format!(
                        "Focus '{focus_id}' step '{}': block_stable_id '{stable_id}' is not in \
                         the building_blocks mirror as an inline_composable block",
                        row.action_id
                    )));
                }
                // Check 2: handler existence, distinct from membership.
                let Some(handler) = crate::conductor::block_dispatch::block_handler(stable_id)
                else {
                    return Err(LifecycleError::ValidationFailed(format!(
                        "Focus '{focus_id}' step '{}': block '{stable_id}' is cataloged but has \
                         no compiled-in handler",
                        row.action_id
                    )));
                };
                let block_customization: serde_json::Value =
                    serde_json::from_str(&row.customization).map_err(|e| {
                        LifecycleError::ValidationFailed(format!(
                            "Focus '{focus_id}' step '{}': customization JSON: {e}",
                            row.action_id
                        ))
                    })?;
                // Check 3: the handler's typed customization struct.
                (handler.validate)(&block_customization).map_err(|e| {
                    LifecycleError::ValidationFailed(format!(
                        "Focus '{focus_id}' step '{}': block '{stable_id}' customization: {e}",
                        row.action_id
                    ))
                })?;
                // LLM-generate-shaped fields are unused for a real block
                // (items.id=496 judgment call 5) -- execute_step()'s
                // dispatch on block_stable_id short-circuits before any of
                // them are read. routing_tier is LocalOnly with no
                // external_access_override: a block step is never external.
                ActionDefinition {
                    action_id: row.action_id.clone(),
                    display_name: row.action_id.clone(),
                    guide_id: default_guide_id.to_owned(),
                    task_type: "general".to_owned(),
                    routing_tier: ExternalAccess::LocalOnly,
                    step_type: StepType::default(),
                    output_var: None,
                    prompt_template: String::new(),
                    field_requirements: HashMap::new(),
                    options_override: HashMap::new(),
                    external_access_override: None,
                    requires_user_handoff: false,
                    revisitable: false,
                    block_stable_id: Some(stable_id.clone()),
                    block_customization: Some(block_customization),
                    schedule_trigger: None,
                    guard: None,
                    declared_inputs: Vec::new(),
                    store_op: None,
                    and_guards: Vec::new(),
                }
            }
        };
        step.schedule_trigger = schedule_trigger;
        step.guard = guard;
        step.and_guards = and_guards;
        steps.push(step);
    }

    // Guard and placement analysis (items.id=878 over items.id=129). Guard
    // variables must come from an earlier row, and every PATH through the
    // guards must hold: no unreachable row, no empty path, a router has a
    // fallback row, and the block-placement rule against the next EXECUTED row.
    //
    // Block placement (items.id=129): a block step's recorded content is its
    // verdict, and last_output() feeds both OUTPUT and the handoff-pause draft,
    // so a block must be followed by an ordinary local step: never last, never
    // directly before an external or handoff step. An unguarded Focus has one
    // path, which is every row in order, so this is the Stage 1 rule unchanged.
    //
    // items.id=879: the declared-inputs lint runs first, so an undeclared name
    // in an external step is reported as that and not as a placement failure.
    //
    // items.id=887: the store writers' own cross-row checks run first, so an
    // argument that names a later row's variable or a derived name is reported
    // as that (conductor::store_op).
    store_op::check_composition(&steps, &row_output_vars(&steps))
        .map_err(|e| LifecycleError::ValidationFailed(format!("Focus '{focus_id}' {e}")))?;
    lint_declared_inputs(focus_id, &steps)?;
    analyze_guarded_composition(focus_id, &steps)?;

    Ok(Some(FocusDefinition {
        focus_id: focus_id.to_owned(),
        display_name: focus_row.display_name,
        description: focus_row.description,
        version: focus_row.version,
        max_routing_tier,
        steps,
        output_type: focus_row.output_type,
        suggest_in_focuses: focus_row.suggest_in_focuses,
        multi_source_validation: focus_row.multi_source_validation,
        generic_title_template: focus_row.generic_title_template,
        high_priority_trigger,
    }))
}

/// items.id=878: LOAD analysis of a composition's guards. Returns the number of
/// paths (1 for an unguarded Focus). See conductor::guard for the rules.
fn analyze_guarded_composition(
    focus_id: &str,
    steps: &[ActionDefinition],
) -> Result<usize, LifecycleError> {
    guarded_composition_paths(focus_id, steps).map(|p| p.len())
}

/// items.id=889: analyze_guarded_composition() returning the enumerated paths
/// themselves (rows run and modelled guard variables), for the acceptance
/// report of the six-intent draft.
fn guarded_composition_paths(
    focus_id: &str,
    steps: &[ActionDefinition],
) -> Result<Vec<guard::Path>, LifecycleError> {
    let row_vars = row_output_vars(steps);
    let shapes: Vec<guard::RowShape> = steps
        .iter()
        .zip(row_vars.iter())
        .map(|(s, row_var)| guard::RowShape {
            action_id: &s.action_id,
            guards: s.guards(),
            output_var: row_var.as_deref(),
            is_block: s.block_stable_id.is_some(),
            local_and_no_handoff: s.routing_tier == ExternalAccess::LocalOnly
                && !s.requires_user_handoff,
            transparent: s.store_op.is_some(),
            reads: s
                .store_op
                .as_ref()
                .map(|o| o.arg_vars().collect())
                .unwrap_or_default(),
            case_write: s.store_op.as_ref().is_some_and(|o| o.op.writes_case()),
            state_var: s.store_op.as_ref().and_then(|o| o.state_var()),
        })
        .collect();
    guard::analyze_composition_paths(&shapes)
        .map_err(|e| LifecycleError::ValidationFailed(format!("Focus '{focus_id}' {e}")))
}

/// The variable each row records when it runs, in row order: an ordinary
/// step's non-empty output_var, or a block handler's declared output variable
/// (ActionDefinition.output_var is None for a block row). Shared by the guard
/// analysis (items.id=878) and the declared-inputs lint (items.id=879).
fn row_output_vars(steps: &[ActionDefinition]) -> Vec<Option<String>> {
    steps
        .iter()
        .map(|s| match s.block_stable_id.as_deref() {
            Some(id) => {
                let handler = crate::conductor::block_dispatch::block_handler(id)?;
                (handler.output_var)(s.block_customization.as_ref()?)
            }
            // items.id=887: a store writer records nothing on the track, but it
            // may publish an id under a pass variable (case_open, delta_record).
            None => match &s.store_op {
                Some(op) => op.published_var().map(str::to_owned),
                None => s.output_var.clone().filter(|v| !v.is_empty()),
            },
        })
        .collect()
}

/// items.id=887: the ActionDefinition of a store-writer row. It is not a model
/// step and not a cataloged block: LocalOnly, no handoff, no template, no output
/// variable of its own. The loader refuses a writer that also cites a block, has
/// a schedule trigger or carries customization, each with its own message.
fn store_writer_step(
    focus_id: &str,
    action_id: &str,
    cites_block: bool,
    has_schedule_trigger: bool,
    customization: &str,
    default_guide_id: &str,
    op: store_op::StoreOp,
) -> Result<ActionDefinition, LifecycleError> {
    let fail = |why: &str| {
        Err(LifecycleError::ValidationFailed(format!(
            "Focus '{focus_id}' step '{action_id}': a store writer {why}"
        )))
    };
    if cites_block {
        return fail("cannot also cite a building block");
    }
    if has_schedule_trigger {
        return fail("cannot carry a schedule trigger");
    }
    let empty = serde_json::from_str::<serde_json::Value>(customization)
        .ok()
        .and_then(|v| v.as_object().map(|o| o.is_empty()))
        .unwrap_or(false);
    if !empty {
        return fail("takes no customization (its arguments are store_args)");
    }
    Ok(ActionDefinition {
        action_id: action_id.to_owned(),
        display_name: action_id.to_owned(),
        guide_id: default_guide_id.to_owned(),
        task_type: "general".to_owned(),
        routing_tier: ExternalAccess::LocalOnly,
        step_type: StepType::default(),
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
        and_guards: Vec::new(),
        declared_inputs: Vec::new(),
        store_op: Some(op),
    })
}

/// items.id=879: the declared-inputs lint (conductor::declared_inputs) over a
/// loaded step list, shared by both loaders.
fn lint_declared_inputs(focus_id: &str, steps: &[ActionDefinition]) -> Result<(), LifecycleError> {
    crate::conductor::declared_inputs::lint(steps, &row_output_vars(steps))
        .map_err(|e| LifecycleError::ValidationFailed(format!("Focus '{focus_id}' {e}")))
}

/// Shared anchor_field+offset -> HighPriorityTrigger parse, used by both
/// focuses.high_priority_trigger_* (Focus-level, decisions.id=712) and
/// focus_block_compositions.schedule_trigger_* (per-step, items.id=496 Q2)
/// -- same two-column-or-neither shape, same parse_offset() underneath.
/// `action_id` is None for the Focus-level call, Some for the per-step call;
/// only used to make a validation error message locate the right row.
fn parse_optional_trigger(
    focus_id: &str,
    action_id: Option<&str>,
    anchor_field: Option<String>,
    offset: Option<String>,
) -> Result<Option<HighPriorityTrigger>, LifecycleError> {
    let Some(anchor_field) = anchor_field else {
        return Ok(None);
    };
    let location = match action_id {
        Some(s) => format!("Focus '{focus_id}' step '{s}'"),
        None => format!("Focus '{focus_id}'"),
    };
    let offset_raw = offset.ok_or_else(|| {
        LifecycleError::ValidationFailed(format!(
            "{location}: a schedule/high-priority trigger's anchor_field is set without an offset"
        ))
    })?;
    let offset = parse_offset(&offset_raw).map_err(|e| {
        LifecycleError::ValidationFailed(format!("{location}: trigger offset: {e}"))
    })?;
    Ok(Some(HighPriorityTrigger {
        anchor_field,
        offset,
    }))
}

/// The (action_id, content) pairs the Phase 5 extraction pass reads
/// (extract_candidates). A block step's content is the block's own advisory
/// verdict, not conversation, so it is left out (items.id=129).
///
/// items.id=886 (decisions.id=939 section 3): a TAINTED step is left out too,
/// one whose prompt named a bound name (session or case content) or, through
/// the closure in conductor::taint, a variable derived from one. Case symptoms,
/// device configuration and the like must not come back out of the generic
/// extraction as "facts about the user".
pub(crate) fn extraction_step_outputs(
    task_track: &TaskTrack,
    focus_def: &FocusDefinition,
) -> Vec<(String, String)> {
    let tainted = crate::conductor::taint::taint_closure(
        &focus_def.steps,
        &row_output_vars(&focus_def.steps),
        crate::conductor::declared_inputs::bound_names(),
    )
    .actions;
    task_track
        .steps()
        .iter()
        .filter(|s| {
            !focus_def
                .steps
                .iter()
                .any(|d| d.action_id == s.action_id && d.block_stable_id.is_some())
        })
        .filter(|s| !tainted.contains(&s.action_id))
        .map(|s| (s.action_id.clone(), s.content.clone()))
        .collect()
}

/// Parse the text of a `.focus` file into a FocusDefinition. The one place
/// YAML text enters the loader (load_focus_definition and the golden tests
/// both go through it).
fn parse_focus_text(text: &str) -> Result<FocusDefinition, LifecycleError> {
    if text.len() > MAX_FOCUS_FILE_BYTES {
        return Err(LifecycleError::ValidationFailed(format!(
            "Focus file is too large (over {MAX_FOCUS_FILE_BYTES} bytes)"
        )));
    }
    let raw: RawFocusFile = serde_saphyr::from_str_with_options(text, focus_yaml_options())
        .map_err(|e| LifecycleError::YamlParse(e.to_string()))?;
    parse_focus_definition(raw)
}

/// Largest .focus file the loader will parse, in bytes. The shipped files are
/// about 6 KiB at most. serde-saphyr's budget does not cap an in-memory
/// string's size (only reader input), so the cap is applied here first.
const MAX_FOCUS_FILE_BYTES: usize = 256 * 1024;

/// Parser options for .focus files (items.id=733, RUSTSEC-2024-0449).
///
/// A .focus file is trusted data that ships with the app, but a Focus can also
/// be user-composed, so the parser runs under explicit limits and any breach
/// is an Err, never a panic or an unbounded allocation:
///   max_depth 16 and flow_nesting_limit 16 (shipped files nest about 5 deep),
///   max_events 50_000 and max_nodes 20_000 (a 256 KiB file cannot reach
///   either by accident), max_total_scalar_bytes 256 KiB, max_documents 1,
///   at most 64 aliases and 64 anchors (the shipped files use none),
///   max_total_comment_bytes 256 KiB.
/// Behaviour matches the serde_yaml 0.9 parser this replaces: a duplicate key
/// is an error, `<<` is an ordinary key (no merge expansion), and only
/// `true` and `false` are booleans, so a word such as `yes` stays a string.
/// Parse errors carry no source snippet, so an authored prompt is never
/// echoed back in an error message.
fn focus_yaml_options() -> serde_saphyr::Options {
    serde_saphyr::options! {
        budget: serde_saphyr::budget! {
            max_depth: 16,
            flow_nesting_limit: 16,
            max_events: 50_000,
            max_nodes: 20_000,
            max_total_scalar_bytes: 256 * 1024,
            max_total_comment_bytes: 256 * 1024,
            max_documents: 1,
            max_aliases: 64,
            max_anchors: 64,
        },
        duplicate_keys: serde_saphyr::DuplicateKeyPolicy::Error,
        merge_keys: serde_saphyr::MergeKeyPolicy::AsOrdinary,
        strict_booleans: true,
        with_snippet: false,
    }
}

/// Parse and validate a Focus definition by focus_id — tries shared.db's
/// composition tables first (items.id=496), falling back to the .focus
/// YAML file when no `focuses` row exists (every Focus shipped before this
/// item, unconditionally). No FocusRun construction required for either
/// path. Extracted from FocusRun::load() (items.id=236) so callers that
/// only need a FocusDefinition (e.g. commands::active_board::
/// get_active_board) aren't forced to construct a full FocusRun (11
/// constructor args) just to read display_config.
pub async fn load_focus_definition(
    pool: &sqlx::SqlitePool,
    focus_id: &str,
) -> Result<FocusDefinition, LifecycleError> {
    if let Some(mut focus_def) = load_focus_definition_from_db(pool, focus_id).await? {
        // items.id=129: a shipped Focus also exists as a bundled .focus file.
        // The loader takes the STRICTER of the DB row's max_routing_tier and
        // the bundled file's, so a wrong seed row (or a tampered shared.db,
        // which is unencrypted) can narrow a shipped Focus's ceiling but can
        // never widen it. Only the bundled and dev-source roots count: the
        // community directory is not a shipped source. A Focus with no
        // bundled file (a composition-authored one) is not cross-checked. A
        // bundled file that cannot be read or parsed fails the load rather
        // than silently skipping the check.
        if let Some(path) = find_shipped_focus_file(focus_id) {
            let text = tokio::fs::read_to_string(&path).await?;
            let shipped = parse_focus_text(&text)?;
            focus_def.max_routing_tier =
                ExternalAccess::min(focus_def.max_routing_tier, shipped.max_routing_tier);
        }
        // items.id=878: and the bundled tier manifest (conductor::tier_manifest),
        // which also covers a database-only shipped Focus that has no .focus
        // file to compare against. A focus_id with no manifest entry is held to
        // the most restrictive tier. Strictest of the three wins.
        focus_def.max_routing_tier = crate::conductor::tier_manifest::apply_manifest(
            &focus_def.focus_id,
            focus_def.max_routing_tier,
        );
        for step in &focus_def.steps {
            let errors = validate_step(step);
            if !errors.is_empty() {
                return Err(LifecycleError::ValidationFailed(format!(
                    "Focus '{}' failed validation:\n{}",
                    focus_def.focus_id,
                    errors
                        .iter()
                        .map(|e| format!("  - {e}"))
                        .collect::<Vec<_>>()
                        .join("\n")
                )));
            }
        }
        return Ok(focus_def);
    }

    let focus_file = find_focus_file(focus_id)?;
    let text = tokio::fs::read_to_string(&focus_file).await?;
    let focus_def = parse_focus_text(&text)?;
    // items.id=879: declared-inputs lint on the YAML path (the DB path runs its
    // own inside load_focus_definition_from_db).
    lint_declared_inputs(&focus_def.focus_id, &focus_def.steps)?;
    // items.id=887: store writers are database-only, so a YAML step is never one
    // and this passes trivially. It runs on both paths so that a YAML field that
    // ever populated store_op could not skip the checks.
    store_op::check_composition(&focus_def.steps, &row_output_vars(&focus_def.steps)).map_err(
        |e| LifecycleError::ValidationFailed(format!("Focus '{}' {e}", focus_def.focus_id)),
    )?;

    let mut all_errors: Vec<String> = Vec::new();
    for step in &focus_def.steps {
        all_errors.extend(validate_step(step));
    }
    if !all_errors.is_empty() {
        return Err(LifecycleError::ValidationFailed(format!(
            "Focus '{}' failed validation:\n{}",
            focus_def.focus_id,
            all_errors
                .iter()
                .map(|e| format!("  - {e}"))
                .collect::<Vec<_>>()
                .join("\n")
        )));
    }

    Ok(focus_def)
}

/// Directory the bundled core Focus files were installed into. Set once from
/// main.rs's setup() via set_focus_dir(), from the Tauri resource dir
/// (bundle.resources maps app/core_artifacts/focuses/*.focus there, items.id=617).
/// Unset in tests and before setup(), in which case the bundled root is skipped.
static FOCUS_DIR: OnceLock<PathBuf> = OnceLock::new();

/// Records the bundled Focus directory. Second and later calls are ignored
/// (OnceLock) with a warning, mirroring privacy_filter::set_backend_dir.
pub fn set_focus_dir(dir: PathBuf) {
    if FOCUS_DIR.set(dir.clone()).is_err() {
        log::warn!(
            "lifecycle: set_focus_dir({dir:?}) ignored -- already set to {:?}",
            FOCUS_DIR.get()
        );
    }
}

/// DEV-ONLY (items.id=617 removed the release use): the source-tree Focus
/// directory, resolved from CARGO_MANIFEST_DIR baked in at compile time. It
/// names a path on the build machine, so a release build never consults it.
/// It is CARGO_MANIFEST_DIR-relative, not CWD-relative, because `cargo tauri
/// dev` always runs the app with CWD=src-tauri (items.id=316).
#[cfg(debug_assertions)]
fn dev_focus_dir() -> Option<PathBuf> {
    Some(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("app")
            .join("core_artifacts")
            .join("focuses"),
    )
}

#[cfg(not(debug_assertions))]
fn dev_focus_dir() -> Option<PathBuf> {
    None
}

/// The bundled (or, in debug builds, dev source tree) `{focus_id}.focus`, if
/// one exists. Unlike find_focus_file_in this ignores the community directory.
fn find_shipped_focus_file(focus_id: &str) -> Option<PathBuf> {
    let filename = format!("{focus_id}.focus");
    let dev = dev_focus_dir();
    let found = [FOCUS_DIR.get().map(PathBuf::as_path), dev.as_deref()]
        .into_iter()
        .flatten()
        .map(|root| root.join(&filename))
        .find(|candidate| candidate.exists());
    found
}

fn find_focus_file(focus_id: &str) -> Result<PathBuf, LifecycleError> {
    let data_root = crate::providers::utils::get_data_root();
    find_focus_file_in(
        focus_id,
        FOCUS_DIR.get().map(PathBuf::as_path),
        &data_root,
        dev_focus_dir().as_deref(),
    )
}

/// Search order: the bundled resource dir, then data_root/community_artifacts/
/// focuses, then (debug builds only) the source tree. The first root that holds
/// `{focus_id}.focus` wins and is logged at info.
fn find_focus_file_in(
    focus_id: &str,
    bundled_dir: Option<&std::path::Path>,
    data_root: &std::path::Path,
    dev_dir: Option<&std::path::Path>,
) -> Result<PathBuf, LifecycleError> {
    let filename = format!("{focus_id}.focus");
    let community_dir = data_root.join("community_artifacts").join("focuses");

    let roots = [
        ("bundled", bundled_dir),
        ("community", Some(community_dir.as_path())),
        ("dev source tree", dev_dir),
    ];
    for (label, root) in roots {
        let Some(root) = root else { continue };
        let candidate = root.join(&filename);
        if candidate.exists() {
            log::info!("lifecycle: loaded Focus '{focus_id}' from {label} root {root:?}");
            return Ok(candidate);
        }
    }
    Err(LifecycleError::FocusNotFound(focus_id.to_owned()))
}

// ---------------------------------------------------------------------------
// FocusDefinition
// ---------------------------------------------------------------------------

/// Internal representation of a parsed .focus YAML file.
/// Populated during Phase 1 LOAD. Immutable thereafter.
/// Python oracle: FocusDefinition frozen dataclass in conductor/lifecycle.py.
///
/// brief: block intentionally absent — deserialized during parsing but never
/// stored or wired to runtime behavior (D6-343).
#[derive(Debug, Clone)]
pub struct FocusDefinition {
    pub focus_id: String,
    pub display_name: String,
    pub description: String,
    pub version: String,
    /// items.id=439 (Part 6, Jason's three-way-fold resolution): the
    /// Focus-author structural ceiling. See FocusRun's
    /// _focus_external_access field for how it combines with
    /// focus_settings.max_permitted_tier. items.id=529: retyped from a raw
    /// u8 to ExternalAccess directly (removing the separate
    /// max_external_access shadow field this used to require).
    pub max_routing_tier: ExternalAccess,
    pub steps: Vec<ActionDefinition>,
    pub output_type: String,
    pub suggest_in_focuses: Vec<String>,
    pub multi_source_validation: bool,
    /// decisions.id=513 (D6-471), items.id=175. Declared in the .focus
    /// file's display_config section; defaults to a platform-wide generic
    /// string when the Focus declares none. Not yet consumed by any caller
    /// -- get_active_board and other Ambient-surface renderers wiring this
    /// in is a separate, later task (this field only carries the value
    /// through parsing).
    pub generic_title_template: String,
    /// decisions.id=712. Active Board high-priority section trigger,
    /// declared once per Focus type in display_config. None when the type
    /// declares no anchor date (most types — see IA_SPEC 2a's collapse-
    /// when-empty behavior). No .focus file in this repo declares one yet;
    /// consumed by commands::active_board::get_active_board.
    pub high_priority_trigger: Option<HighPriorityTrigger>,
}

/// decisions.id=712: a Focus type's Active Board high-priority section
/// trigger — a signed time offset relative to that type's own anchor date
/// field (identified by anchor_field, a key into a Topic's extra_metadata).
#[derive(Debug, Clone, PartialEq)]
pub struct HighPriorityTrigger {
    pub anchor_field: String,
    pub offset: Duration,
}

impl HighPriorityTrigger {
    /// One-sided window: true from anchor+offset onward, no upper bound.
    /// Exit happens via the topic's own lifecycle transition (e.g. marked
    /// complete), not via the clock — see items.id=236 plan judgment call 4.
    pub fn is_active(&self, anchor: DateTime<Utc>, now: DateTime<Utc>) -> bool {
        now >= anchor + self.offset
    }
}

// ---------------------------------------------------------------------------
// RunResult
// ---------------------------------------------------------------------------

/// Structured outcome of a focus run or phase call.
/// Python oracle: RunResult dataclass in conductor/lifecycle.py.
#[derive(Debug, Clone, Serialize)]
pub struct RunResult {
    pub focus_run_id: String,
    pub status: String,
    pub output_id: Option<String>,
    pub output_content: Option<String>,
    pub failure: Option<FailureResult>,
    /// R1 crisis-handling floor (decisions.id=607, items.id=265, items.id=297):
    /// Some(crisis::resource_block()) iff crisis_floor_triggered was set on
    /// this run, at every construction site reachable from EXECUTE (cloud_frontier
    /// boundary, handle_step_failure(), output()). Phase 1/2 (LOAD/AUTHORIZE)
    /// failure paths deliberately do not populate this — see crisis.rs module
    /// doc for the scope boundary.
    pub crisis_resource_block: Option<String>,
}

// ---------------------------------------------------------------------------
// RunStatusPayload
// ---------------------------------------------------------------------------

/// Push event payload for the "run-status-update" Tauri event.
/// Emitted at step boundaries and phase transitions via AppHandle::emit().
/// Python oracle: N/A — Rust-only IPC push event (no Python equivalent).
/// IPC surface: HANDOFF_IPC_SURFACE.md push event — confirm event name on
/// IPC wire-up in Layer 8.
#[derive(Debug, Clone, Serialize, specta::Type)]
pub struct RunStatusPayload {
    pub focus_run_id: String,
    pub status: String,
    pub current_step: usize,
    pub total_steps: usize,
    /// Step display name for frontend progress rendering, e.g. "Running: Generate outline".
    /// None during phase transitions (initialize, output, cleanup, error handlers)
    /// where no specific step is active. Some(&step.display_name) at each step boundary.
    /// IPC surface: HANDOFF_IPC_SURFACE.md push event — align with frontend on field name.
    pub step_display_name: Option<String>,
    /// The just-completed step's real generated content (TaskStep.content),
    /// for ChatPane.tsx's staged/incremental reveal (items.id=245-ish).
    /// Some(...) only on the step-COMPLETION emission inside the EXECUTE
    /// loop (execute() below, right after execute_step() succeeds) — every
    /// other emit_status call site, including the step-START announcement
    /// that fires at the top of the same loop iteration, passes None here.
    pub step_content: Option<String>,
    /// R1 crisis-handling floor (items.id=297): mirrors RunResult's field of
    /// the same name, carried over the live "run-status-update" push event so
    /// the frontend can show the block the moment a run pauses/fails, without
    /// waiting on the awaited IPC call that never actually carries RunResult
    /// back to the UI on any current call path (see lifecycle.rs's
    /// handle_step_failure() and the cloud_frontier boundary in execute()). Some(...)
    /// only on the same emit call as a "failed"/"awaiting_user" transition
    /// for a crisis-flagged run; None otherwise.
    pub crisis_resource_block: Option<String>,
    /// Cross-Persona entity_facts omitted from this run's context
    /// (decisions.id=815, items.id=27) — declined, or caught by the
    /// documented pre-run-query/INITIALIZE-read race decisions.id=639
    /// accepts. Same "travels over the live push event" rationale as
    /// crisis_resource_block above: RunResult never reaches the frontend on
    /// any current call path (submit_focus_run spawns-and-forgets it;
    /// send_message awaits it internally but returns Vec<MessageInfo>, not
    /// RunResult). Empty when nothing was omitted. Populated from
    /// self.personal_track — see emit_status_with_content.
    pub provenance_omissions: Vec<crate::conductor::types::OmittedCrossPersonaFact>,
    /// items.id=735 (decisions.id=883): Gate 2 found a withheld personal value
    /// in the just-completed step's reply. A plain, non-blocking notice; the
    /// reply is intact and the run continues. Some(true) only on the
    /// step-COMPLETION emission of a flagged step, None on every other emit.
    /// Read from the step's TaskStep, so it travels with the content it
    /// concerns.
    pub personal_flagged: Option<bool>,
    /// items.id=735: the flagged reply's disclosure-log entry could not be
    /// written. Some(true) only alongside personal_flagged, None otherwise.
    pub audit_unavailable: Option<bool>,
}

// ---------------------------------------------------------------------------
// Module-local DB helpers (private)
// ---------------------------------------------------------------------------

/// Open outputs.db with SQLCipher key for focus_run and snapshot table access.
/// Python oracle: open_outputs_db(user_id, persona_id, key_hex) context manager.
async fn open_outputs_db(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
) -> Result<SqliteConnection, LifecycleError> {
    crate::auth::epoch_guard::ensure_current(user_id).await?;
    let path = db_path_outputs(user_id, persona_id);

    if !path.exists() {
        crate::persistence::migrations::migrate_outputs_db(user_id, persona_id, key_hex)
            .await
            .map_err(|e| LifecycleError::DatabaseMigration(e.to_string()))?;
    }

    let db_key = crate::auth::subkeys::db_key_hex(
        user_id,
        key_hex,
        crate::auth::subkeys::DbPurpose::Outputs,
        persona_id,
    )?;
    let conn = connect_options_encrypted(&path, &db_key)
        .create_if_missing(false)
        .pragma("busy_timeout", "5000")
        .connect()
        .await?;
    Ok(conn)
}

// ---------------------------------------------------------------------------
// FocusRun
// ---------------------------------------------------------------------------

/// Orchestrates a single focus run through all seven lifecycle phases.
///
/// Owns PersonalTrack, TaskTrack, and SharedStateTrack (D6-342 actor model).
/// Single Tokio task — no cross-task sharing, no Arc<Mutex<>> for track ownership.
///
/// current_step: usize — explicit step index, not an implicit counter (D6-347).
///   Persisted to checkpoint; makes mid-run resume correct by construction.
///
/// app_handle: None in tests; Some in production.
///   When Some: emits run_status_update push events at step boundaries (D6-345).
///   When None: emit is a silent no-op; step progress is logged via log::debug.
///
/// privacy_gateway: PrivacyGateway<L> — L defaults to SqliteDisclosureLogger.
///   Production gets the concrete, disclosure_log-table-backed logger by
///   default (items.id=173). Tests inject FocusRun<TestLogger> or
///   FocusRun<NoopLogger> explicitly. See the module doc comment's
///   "Privacy gateway" section for the full rationale.
pub struct FocusRun<L: DisclosureLoggerForRun = SqliteDisclosureLogger> {
    // Constructor parameters
    pub user_id: String,
    pub persona_id: String,
    pub focus_id: String,
    /// shared.db pool (items.id=483) -- every shared.db read this struct's
    /// methods make (focus_settings/personas tier ceiling, provider/
    /// preference lookups, artifact_versions) goes through this, replacing
    /// what used to be per-call open_instance_db() connections.
    pub pool: sqlx::SqlitePool,
    pub scheduler: Arc<ConductorScheduler>,
    pub user_input: String,
    pub is_fast_lane: bool,
    pub key_hex: Option<String>,
    pub topic_id: Option<String>,
    pub is_quick_ask: bool,

    // Cross-Persona Data Provenance: entity_facts.id values the user confirmed
    // via the pre-Focus-start IPC flow (decisions.id=546, decisions.id=639,
    // items.id=27). Populated by the frontend calling
    // get_pending_cross_persona_confirmations() BEFORE constructing this
    // FocusRun, then passed straight through here -- never mutated once the
    // run starts, matching decisions.id=546's "per-session, non-persisted"
    // confirmation. Any cross_persona_export=true fact whose id is not in
    // this set is treated as declined (omitted, not hard-blocked -- Jason,
    // items.id=27 session 2026-07-25).
    pub confirmed_cross_persona_fact_ids: std::collections::HashSet<String>,

    // Tauri AppHandle for push events — None in tests, Some in production (D6-345)
    pub app_handle: Option<tauri::AppHandle<tauri::Wry>>,

    /// Test-only recorder of every run-status-update payload this run would
    /// emit (items.id=129): there is no AppHandle in tests, so this is how a
    /// test observes which step boundaries emit.
    #[cfg(test)]
    pub(crate) emitted_for_test: std::sync::Mutex<Vec<RunStatusPayload>>,

    // State populated during lifecycle phases (all Option — None until populated)
    pub focus_run_id: Option<String>,
    pub focus_def: Option<FocusDefinition>,
    pub personal_track: Option<PersonalTrack>,
    pub task_track: Option<TaskTrack>,
    pub shared_state: Option<SharedStateTrack>,
    pub failure_handler: Option<FailureHandler>,
    pub privacy_gateway: Option<PrivacyGateway<L>>,

    // Tier configuration (set at AUTHORIZE, used throughout EXECUTE)
    /// items.id=529: retyped from u8 to ExternalAccess directly (focus_settings
    /// storage is string-native now; see get_focus_tier_ceiling()). Still
    /// bridged to a raw u8 for the numeric execution_tier axis's own
    /// downstream consumers (MemoryBroker::assemble_context(), scan_output())
    /// via execution_tier_ordinal() -- that separate axis is out of this
    /// item's scope (items.id=528 territory).
    _focus_max_permitted_tier: ExternalAccess,
    _focus_privacy_tier: u8,
    /// items.id=439: the Focus-level external_access ceiling, excluding any
    /// step's own override -- min(_focus_max_permitted_tier,
    /// focus_def.max_routing_tier). Set at AUTHORIZE (Jason's
    /// three-way-fold resolution; see plan doc), used by authorize()'s
    /// per-step tighten-only check, build_failure_handler(), and threaded into
    /// every StepContext as focus_external_access.
    _focus_external_access: ExternalAccess,

    // Run execution state
    _output_id: Option<String>,

    /// Explicit step index (D6-347). Not an implicit counter.
    /// Updated at the start of each step iteration for correct resume behavior.
    pub current_step: usize,

    /// True when a F8 (SnapshotWrite) failure has suspended checkpointing.
    _checkpointing_suspended: bool,

    /// Rendered MemoryBroker context string for this session.
    /// Assembled at Phase 3 INITIALIZE. Cleared at Phase 7 CLEANUP.
    _persona_context_rendered: String,

    /// R1 crisis-handling floor (decisions.id=607, items.id=265). Set by the
    /// caller (commands::execution::load_and_authorize_run) right after
    /// construction, from a local/deterministic check (conductor::crisis::detect)
    /// on the actual fresh user turn -- never client-supplied over IPC, never
    /// computed from the blended conversation-history window. Defaults false;
    /// not part of the constructor argument list so existing call sites
    /// (including the six test helpers in this file) are unaffected.
    /// Consumed once, at Phase 5 OUTPUT (see output()); never persisted.
    pub(crate) crisis_floor_triggered: bool,

    /// Whether a user is present for this run (items.id=710, review ruling 1).
    /// Defaults to `Unattended` in `new` and in `rehydrate_focus_run`: a run
    /// whose attendance is not stated parks instead of spending beyond an
    /// approved per-run budget (no-autonomous-action tenet, spec Part 8e).
    /// The IPC commands that a user triggers (commands::execution) set
    /// `Attended` after construction, like `crisis_floor_triggered`. The
    /// scheduled sweep leaves the default: it has no user in the loop. A
    /// future second unattended path inherits the safe default with no code
    /// change; a real attended flag is its own follow-up.
    pub(crate) attendance: crate::spend::gate::RunAttendance,
    /// items.id=715 (decisions.id=874, 883): set by the owner of an API Chat
    /// run so its hosted steps stream their reply (display-only deltas, a
    /// cancel token, Gate 2 flags instead of holding). `None` - the default,
    /// and what every other run keeps - sends `stream: false` exactly as
    /// before. A rehydrated or swept run has none. Local steps ignore it.
    pub chat_stream: Option<crate::chat_stream::ChatStream>,
    /// items.id=716: the provider and model an API Chat run is bound to.
    /// `None` (the default, and what every other run keeps) resolves the
    /// provider from the user's preference exactly as before. Must be set
    /// BEFORE `authorize()`, which builds the failure handler from provider
    /// resolution. A rehydrated run re-derives it from its conversation's
    /// binding.
    pub chat_binding: Option<crate::api_chat::BoundDestination>,
    /// items.id=886 (decisions.id=939 section 2): the chat this run belongs to,
    /// the key of its session variables and one-time write latch. Set by the
    /// SERVER, as a parameter of load_and_authorize_run, never from the frontend
    /// request DTO. None for a run that belongs to no chat (submit_focus_run, a
    /// rehydrated or swept run): a session name then reads as absent, and
    /// write_mode, which cannot be read without a chat, fails the pass closed.
    /// items.id=888 (resume pin) must carry it across a resume.
    pub context_key: Option<String>,
    /// items.id=886: the seeds read at the start of the current pass and the
    /// names a step sees only when its own template names them. Empty until
    /// execute() seeds it.
    pub(crate) store_tokens: crate::conductor::taint::StoreTokens,
    /// items.id=887: ids that store writers published during this pass
    /// (case_open and delta_record, under the row's store_target), for later
    /// writers' arguments and guards. Pass-local: never a TaskTrack step, never
    /// checkpointed, never in a prompt or in extraction.
    pub(crate) store_vars: HashMap<String, String>,
    /// items.id=778 (decisions.id=896): this run's writer lease. Taken by the
    /// caller that starts or resumes the run, BEFORE the run can touch a
    /// database, and held until the run is dropped, which for a spawned run is
    /// after `finalize_chat_reply`: a key rotation waits for it, or stops the
    /// run at the user's choice. `None` only for runs built by tests. Set only
    /// through `attach_lease` (a source-scan test in auth/writer_gate.rs
    /// enforces it), which also binds the lease to this run's id.
    pub(crate) lease: Option<crate::auth::writer_gate::WriterLease>,
}

impl<L: DisclosureLoggerForRun> FocusRun<L> {
    #[allow(clippy::too_many_arguments)] // Explicit architecture boundary; see D6-342/D6-346.
    pub fn new(
        user_id: String,
        persona_id: String,
        focus_id: String,
        pool: sqlx::SqlitePool,
        scheduler: Arc<ConductorScheduler>,
        user_input: String,
        is_fast_lane: bool,
        key_hex: Option<String>,
        topic_id: Option<String>,
        is_quick_ask: bool,
        confirmed_cross_persona_fact_ids: std::collections::HashSet<String>,
        app_handle: Option<tauri::AppHandle<tauri::Wry>>,
    ) -> Self {
        Self {
            user_id,
            persona_id,
            focus_id,
            pool,
            scheduler,
            user_input,
            is_fast_lane,
            key_hex,
            topic_id,
            is_quick_ask,
            confirmed_cross_persona_fact_ids,
            app_handle,
            #[cfg(test)]
            emitted_for_test: std::sync::Mutex::new(Vec::new()),
            focus_run_id: None,
            focus_def: None,
            personal_track: None,
            task_track: None,
            shared_state: None,
            failure_handler: None,
            privacy_gateway: None,
            _focus_max_permitted_tier: ExternalAccess::LocalOnly,
            _focus_privacy_tier: 1,
            _focus_external_access: ExternalAccess::LocalOnly,
            _output_id: None,
            current_step: 0,
            _checkpointing_suspended: false,
            _persona_context_rendered: String::new(),
            crisis_floor_triggered: false,
            attendance: crate::spend::gate::RunAttendance::Unattended,
            chat_stream: None,
            chat_binding: None,
            context_key: None,
            store_tokens: crate::conductor::taint::StoreTokens::default(),
            store_vars: HashMap::new(),
            lease: None,
        }
    }

    // =========================================================================
    // Emit helper
    // =========================================================================

    /// Emit a run_status_update push event. Non-fatal — errors are logged only.
    /// D6-345: fired at step boundaries and all phase transitions.
    ///
    /// step_display_name: Some(&step.display_name) at step boundaries;
    ///   None during phase transitions where no specific step is active.
    fn emit_status(&self, status: &str, step_display_name: Option<&str>) {
        self.emit_status_with_content(status, step_display_name, None, None);
    }

    /// step_content: the just-completed step's real generated content
    /// (TaskStep.content, via task_track.last_output()). Passed by the
    /// step-completion call site in execute(), and by the cloud_frontier boundary
    /// pause (items.id=317 -- the last completed step's output is the draft
    /// awaiting Gate3 review); every other caller (including plain
    /// emit_status above) passes None.
    ///
    /// crisis_resource_block: R1 crisis-handling floor (items.id=297). Some(...)
    /// only on the same "failed"/"awaiting_user" emit that accompanies a
    /// crisis-flagged run's cloud_frontier pause or handle_step_failure() exit — see
    /// RunStatusPayload's own field doc for why this travels over the push
    /// event rather than relying solely on RunResult.
    fn emit_status_with_content(
        &self,
        status: &str,
        step_display_name: Option<&str>,
        step_content: Option<&str>,
        crisis_resource_block: Option<&str>,
    ) {
        self.emit_status_payload(
            status,
            step_display_name,
            step_content,
            crisis_resource_block,
            None,
        );
    }

    /// The step-completion emit: like emit_status_with_content, plus the
    /// Gate 2 flags of the step just recorded (items.id=735), read from the
    /// TaskTrack so they travel with the content.
    fn emit_step_complete(&self, step_display_name: &str, step_content: Option<&str>) {
        self.emit_status_payload(
            "running",
            Some(step_display_name),
            step_content,
            None,
            self.last_step_reply_flags(),
        );
    }

    /// Some(audit_unavailable) iff the step most recently recorded in the
    /// TaskTrack was flagged by Gate 2 (items.id=735); None otherwise.
    fn last_step_reply_flags(&self) -> Option<bool> {
        self.task_track
            .as_ref()
            .and_then(|tt| tt.steps().last())
            .filter(|s| s.personal_flagged)
            .map(|s| s.audit_unavailable)
    }

    /// `reply_flags`: Some(audit_unavailable) iff the step just completed was
    /// flagged by Gate 2; None on every other emit.
    fn emit_status_payload(
        &self,
        status: &str,
        step_display_name: Option<&str>,
        step_content: Option<&str>,
        crisis_resource_block: Option<&str>,
        reply_flags: Option<bool>,
    ) {
        #[cfg(test)]
        self.emitted_for_test
            .lock()
            .unwrap()
            .push(self.build_status_payload(
                status,
                step_display_name,
                step_content,
                crisis_resource_block,
                reply_flags,
            ));
        let Some(handle) = &self.app_handle else {
            return; // no handle in tests — silent no-op
        };
        let payload = self.build_status_payload(
            status,
            step_display_name,
            step_content,
            crisis_resource_block,
            reply_flags,
        );
        use tauri::Emitter;
        if let Err(e) = handle.emit("run-status-update", &payload) {
            log::warn!("lifecycle: emit run-status-update failed: {e}");
        }
    }

    /// Build the run-status-update payload. Separate from the emit so a test
    /// can check it without an AppHandle.
    fn build_status_payload(
        &self,
        status: &str,
        step_display_name: Option<&str>,
        step_content: Option<&str>,
        crisis_resource_block: Option<&str>,
        reply_flags: Option<bool>,
    ) -> RunStatusPayload {
        let total = self.focus_def.as_ref().map(|d| d.steps.len()).unwrap_or(0);
        let provenance_omissions = self
            .personal_track
            .as_ref()
            .map(|t| t.omitted_cross_persona_facts().to_vec())
            .unwrap_or_default();
        RunStatusPayload {
            focus_run_id: self.focus_run_id.clone().unwrap_or_default(),
            status: status.to_owned(),
            current_step: self.current_step,
            total_steps: total,
            step_display_name: step_display_name.map(|s| s.to_owned()),
            step_content: step_content.map(|s| s.to_owned()),
            crisis_resource_block: crisis_resource_block.map(|s| s.to_owned()),
            provenance_omissions,
            personal_flagged: reply_flags.map(|_| true),
            audit_unavailable: reply_flags.filter(|unavailable| *unavailable),
        }
    }

    // =========================================================================
    // Phase 1 — LOAD
    // =========================================================================

    /// Parse the Focus definition and validate all steps. Populates
    /// self.focus_def. items.id=496: now tries shared.db's composition
    /// tables via self.pool before falling back to the .focus YAML file --
    /// see load_focus_definition()'s own doc comment. Every Focus shipped
    /// before this item has no shared.db `focuses` row, so this remains a
    /// pure file read for all of them.
    /// Python oracle: FocusRun.load()
    pub async fn load(&mut self) -> Result<(), LifecycleError> {
        self.focus_def = Some(load_focus_definition(&self.pool, &self.focus_id).await?);
        Ok(())
    }

    // =========================================================================
    // Phase 2 — AUTHORIZE
    // =========================================================================

    /// Verify tier permissions and key; create focus_run record at status=initializing.
    /// Python oracle: FocusRun.authorize()
    pub async fn authorize(&mut self) -> Result<(), LifecycleError> {
        let _ = self
            .focus_def
            .as_ref()
            .expect("authorize() requires load() to have succeeded first");

        // Key presence check — PersonalDBDecryptionError equivalent.
        let key_hex = self.key_hex.as_deref().unwrap_or("");
        if key_hex.is_empty() {
            return Err(LifecycleError::NoKey);
        }

        self.resolve_tier_config().await?;
        self.failure_handler = Some(self.build_failure_handler().await);
        self.focus_run_id = Some(Uuid::new_v4().to_string());
        self.write_focus_run_record("initializing").await?;
        Ok(())
    }

    /// FailureHandler for this run (items.id=692). Escalation offers are
    /// suppressed only when provider selection would refuse them: the user's
    /// hosted provider is excluded by the Focus-level ceiling. A user with no
    /// provider set up (NotConfigured) still gets the offer -- accepting it
    /// re-runs the step, which reaches the F10 setup guidance. Requires
    /// resolve_tier_config() to have run (reads _focus_external_access).
    async fn build_failure_handler(&self) -> FailureHandler {
        use crate::conductor::provider_selection::{
            resolve_external_provider, ExternalProviderResolution,
        };
        let escalation_available = self._focus_external_access == ExternalAccess::LocalOnly
            || !matches!(
                match &self.chat_binding {
                    // items.id=716: a bound run asks about its own provider.
                    Some(bound) => {
                        crate::conductor::provider_selection::resolve_bound_provider(
                            &self.pool,
                            &bound.provider_id,
                            self._focus_external_access,
                        )
                        .await
                    }
                    None => {
                        resolve_external_provider(
                            &self.pool,
                            &self.user_id,
                            self.key_hex.as_deref().unwrap_or(""),
                            &self.persona_id,
                            &self.focus_id,
                            self._focus_external_access,
                        )
                        .await
                    }
                },
                ExternalProviderResolution::ExcludedByCeiling { .. }
            );
        FailureHandler::with_escalation(self._focus_external_access, escalation_available)
    }

    /// Compute and store the Focus-level tier ceiling
    /// (_focus_max_permitted_tier/_focus_privacy_tier/_focus_external_access),
    /// validating that no step's external_access_override exceeds it.
    ///
    /// Shared by authorize() (Phase 2, a brand-new run) and
    /// rehydrate_focus_run() (items.id=574, reconstructing a resumed run) —
    /// same computation, same violation check, two different entry points
    /// into an in-memory FocusRun. Requires focus_def to already be set
    /// (load() must have run first).
    async fn resolve_tier_config(&mut self) -> Result<(), LifecycleError> {
        let (max_permitted, privacy_tier) = self.get_focus_tier_ceiling().await?;
        self._focus_max_permitted_tier = max_permitted;
        self._focus_privacy_tier = privacy_tier;

        let focus_def = self
            .focus_def
            .as_ref()
            .expect("resolve_tier_config() requires focus_def");

        // items.id=439 (Part 6, Jason's three-way-fold resolution): the
        // Focus-level ceiling excluding any step's own override -- folds the
        // user preference (focus_settings.max_permitted_tier) together with
        // the Focus-author structural ceiling (focus_def.max_routing_tier)
        // without collapsing either into a single legacy number.
        self._focus_external_access = max_permitted.min(focus_def.max_routing_tier);

        for step in &focus_def.steps {
            if let Some(override_) = step.external_access_override {
                if override_ > self._focus_external_access {
                    return Err(LifecycleError::TierViolation(format!(
                        "Step '{}' requires external access '{}' but focus ceiling is '{}'.",
                        step.action_id,
                        override_.as_str(),
                        self._focus_external_access.as_str()
                    )));
                }
            }
        }

        Ok(())
    }

    /// Read (max_permitted_tier, privacy_tier) from focus_settings.
    /// Asserts focus_settings row exists — missing row is a hard error (D6-303).
    /// Python oracle: FocusRun._get_focus_tier_ceiling()
    async fn get_focus_tier_ceiling(&self) -> Result<(ExternalAccess, u8), LifecycleError> {
        use crate::persistence::focus_settings_store::get_focus_settings;
        use crate::persistence::persona_store::is_user_in_persona;

        // Membership only: the Persona's name is in the encrypted registry
        // and AUTHORIZE has no use for it (items.id=661).
        if !is_user_in_persona(&self.pool, &self.user_id, &self.persona_id)
            .await
            .map_err(|e| LifecycleError::PersonaStore(e.to_string()))?
        {
            return Err(LifecycleError::PersonaNotFound(format!(
                "Persona '{}' not found for user '{}'.",
                self.persona_id, self.user_id
            )));
        }

        let settings = get_focus_settings(&self.pool, &self.persona_id, &self.focus_id)
            .await
            .map_err(|e| LifecycleError::FocusSettingsStore(e.to_string()))?
            .ok_or_else(|| {
                LifecycleError::FocusSettingsNotFound(format!(
                    "Focus settings not found for persona='{}' focus='{}'. \
                     Configure this Focus before running.",
                    self.persona_id, self.focus_id
                ))
            })?;

        // items.id=448 retyped FocusSettings.max_permitted_tier from i32 to
        // ExternalAccess. items.id=529: storage itself is string-native now
        // (shared_020.sql), so this returns the enum directly -- no more
        // legacy round-trip, and AnonymousPreferred is a real, storable
        // value here for the first time. The numeric execution_tier
        // ceiling calc in this file (execute_step()'s Axis-1) is a
        // deliberately separate axis, bridged via ExternalAccess::min() +
        // execution_tier_ordinal(), out of this item's scope to unify
        // further (items.id=528 territory).
        Ok((settings.max_permitted_tier, settings.privacy_tier as u8))
    }

    /// Write or update the focus_run record in outputs.db.
    /// INSERT on first call (status=initializing); UPDATE on subsequent calls.
    /// Python oracle: FocusRun._write_focus_run_record()
    async fn write_focus_run_record(&self, status: &str) -> Result<(), LifecycleError> {
        let focus_run_id = self
            .focus_run_id
            .as_deref()
            .expect("write_focus_run_record() requires focus_run_id to be set");
        let key_hex = self
            .key_hex
            .as_deref()
            .expect("write_focus_run_record() requires key_hex");

        let mut conn = open_outputs_db(&self.user_id, &self.persona_id, key_hex).await?;

        let existing: bool = sqlx::query("SELECT id FROM focus_runs WHERE id = ?")
            .bind(focus_run_id)
            .fetch_optional(&mut conn)
            .await?
            .is_some();

        if existing {
            sqlx::query("UPDATE focus_runs SET status = ? WHERE id = ?")
                .bind(status)
                .bind(focus_run_id)
                .execute(&mut conn)
                .await?;
        } else {
            sqlx::query(
                "INSERT INTO focus_runs
                 (id, focus_id, status, is_fast_lane, is_quick_ask,
                  topic_id, started_at, notes)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(focus_run_id)
            .bind(&self.focus_id)
            .bind(status)
            .bind(if self.is_fast_lane { 1i32 } else { 0i32 })
            .bind(if self.is_quick_ask { 1i32 } else { 0i32 })
            .bind(self.topic_id.as_deref())
            .bind(now())
            .bind("{}")
            .execute(&mut conn)
            .await?;
        }

        Ok(())
    }

    /// Bounded wrapper around write_focus_run_record() for the failure/
    /// terminal-status call sites. A stalled DB write (lock contention or raw
    /// disk I/O latency, e.g. under concurrent backup I/O) must never hang the
    /// run indefinitely -- 10s is chosen to sit above the 5s busy_timeout so a
    /// legitimate lock wait can still complete normally. A transient failure is
    /// retried; a failure that remains is NOT silent (items.id=778): the run's
    /// final status would otherwise stay `running`, so it is reported to the
    /// user (`data-write-failed`, kind `run`) as well as logged.
    async fn write_focus_run_record_logged(&self, status: &str) {
        let saved = crate::write_failure::retry_transient(|| async {
            match tokio::time::timeout(
                std::time::Duration::from_secs(10),
                self.write_focus_run_record(status),
            )
            .await
            {
                Ok(Ok(())) => Ok(()),
                Ok(Err(e)) => Err(e.to_string()),
                Err(_) => Err("timed out after 10s".to_owned()),
            }
        })
        .await;
        if let Err(e) = saved {
            crate::write_failure::report(
                self.app_handle.as_ref(),
                crate::write_failure::Lost::Run,
                &format!("lifecycle: write_focus_run_record({status}) failed: {e}"),
            );
        }
    }

    // =========================================================================
    // Phase 3 — INITIALIZE
    // =========================================================================

    /// Build tracks; construct PrivacyGateway; assemble persona context; promote to running.
    /// Python oracle: FocusRun.initialize()
    pub async fn initialize(&mut self) -> Result<(), LifecycleError> {
        let mut personal_track = self.build_personal_track().await?;
        personal_track.seal();
        self.personal_track = Some(personal_track);
        self.task_track = Some(TaskTrack::new());
        self.shared_state = Some(SharedStateTrack::new());
        self.privacy_gateway = Some(PrivacyGateway::new(L::for_run(
            &self.user_id,
            &self.persona_id,
            self.key_hex.as_deref().unwrap_or(""),
        )));
        self._persona_context_rendered = self.assemble_persona_context().await;
        self.write_focus_run_record("running").await?;
        self.emit_status("running", None);
        Ok(())
    }

    /// Call MemoryBroker to assemble context slice; render to string; clear slice.
    /// Returns empty string on failure — non-fatal for unnamed/Quick Ask sessions.
    /// Python oracle: FocusRun._assemble_persona_context()
    async fn assemble_persona_context(&self) -> String {
        let key_hex = match self.key_hex.as_deref() {
            Some(k) if !k.is_empty() => k,
            _ => return String::new(),
        };

        let model_context_window = std::env::var("QR_DEFAULT_CONTEXT_WINDOW")
            .ok()
            .and_then(|v| v.parse::<i32>().ok())
            .unwrap_or(8192);

        let broker = MemoryBroker::new();
        let mut slice = broker
            .assemble_context(
                &self.user_id,
                &self.persona_id,
                &self.focus_id,
                self.topic_id.as_deref(),
                key_hex,
                execution_tier_ordinal(self._focus_max_permitted_tier) as i32,
                model_context_window,
                self.is_quick_ask,
                None, // tier_a_ceiling — use QR_TIER_A_TOKEN_CEILING env default
                None, // reserve_margin — use QR_MEMORY_RESERVE_MARGIN env default
            )
            .await;

        let rendered = slice.render();
        slice.clear();
        rendered
    }

    /// Load PersonalTrack from personal.db and attach guide/operator versions.
    /// Returns an UNSEALED track — initialize() seals it immediately after.
    /// Python oracle: FocusRun._build_personal_track()
    async fn build_personal_track(&self) -> Result<PersonalTrack, LifecycleError> {
        use crate::persistence::personal_store::{
            load_entity_facts_for_context, load_personal_track,
        };

        let key_hex = self.key_hex.as_deref().unwrap_or("");
        let mut track = load_personal_track(&self.user_id, &self.persona_id, key_hex)
            .await
            .map_err(|e| LifecycleError::PersonalStore(e.to_string()))?;

        // Cross-Persona Data Provenance read path (decisions.id=546, items.id=27).
        // Loads entity_facts rows and runs the decisions.id=424 enforcement
        // check on each (apply_entity_fact_provenance_check) — same-Persona
        // facts include, cross-Persona facts include only if their id is in
        // self.confirmed_cross_persona_fact_ids (decisions.id=639's
        // pre-Focus-start IPC confirmation, resolved before this FocusRun
        // was even constructed), mismatched provenance hard-blocks.
        let entity_facts = load_entity_facts_for_context(&self.user_id, &self.persona_id, key_hex)
            .await
            .map_err(|e| LifecycleError::PersonalStore(e.to_string()))?;
        for fact in entity_facts {
            self.apply_entity_fact_provenance_check(&mut track, fact)
                .await?;
        }

        self.load_group_facts_into_track(&mut track, key_hex)
            .await?;

        let focus_def = self
            .focus_def
            .as_ref()
            .expect("build_personal_track() requires focus_def");

        let mut guide_ids: Vec<String> =
            focus_def.steps.iter().map(|s| s.guide_id.clone()).collect();
        guide_ids.sort();
        guide_ids.dedup();

        let mut versions = self.load_guide_versions(&guide_ids).await;
        versions.extend(self.load_operator_versions().await);

        track
            .set_source_versions(versions)
            .map_err(|e| LifecycleError::PersonalStore(e.to_string()))?;

        Ok(track)
    }

    /// items.id=296 (GROUP_DB_DESIGN_20260802.md Section 3.3): load this
    /// run's group facts into `track`, for every group_id this persona is
    /// opted into (personal.db's group_fact_sources, personal_006.sql) AND
    /// currently holds a *resident* key for in auth::registry::
    /// GroupKeyRegistry. Both conditions gate independently — the opt-in
    /// alone is not sufficient (a group whose key isn't resident this
    /// session is silently skipped, not an error: "no key, no access, full
    /// stop" per Section 3.3 point 4). No provenance branching the way
    /// apply_entity_fact_provenance_check needs — group facts have no
    /// cross-persona-export concept; the resident-key check (cryptographic)
    /// combined with the opt-in (this function's own two conditions) IS the
    /// complete gate, no second stacked check.
    ///
    /// GroupKeyRegistry access via self.app_handle rather than a new
    /// FocusRun field/constructor parameter: main.rs's own periodic
    /// folder-sync pull loop already resolves GroupKeyRegistry the same way
    /// (`pull_handle.state::<GroupKeyRegistry>()` off a cloned AppHandle) —
    /// reusing that avoids touching FocusRun::new()'s signature (and its 4
    /// test call sites + 2 real command call sites) for a value already
    /// reachable through a field FocusRun already carries. app_handle is
    /// `None` in every existing test (matching this file's own doc comment
    /// on that field), so this step is correctly a no-op there — same
    /// graceful-degradation shape push-event emission already has when
    /// app_handle is absent.
    async fn load_group_facts_into_track(
        &self,
        track: &mut PersonalTrack,
        key_hex: &str,
    ) -> Result<(), LifecycleError> {
        use crate::auth::registry::GroupKeyRegistry;
        use crate::persistence::{group_fact_sources_store, group_fact_store};

        let Some(app_handle) = self.app_handle.as_ref() else {
            return Ok(());
        };
        let group_key_registry = app_handle.state::<GroupKeyRegistry>();

        let opted_in_group_ids = group_fact_sources_store::list_group_fact_sources(
            &self.user_id,
            &self.persona_id,
            key_hex,
        )
        .await
        .map_err(|e| LifecycleError::PersonalStore(e.to_string()))?;

        for group_id in opted_in_group_ids {
            let Some(group_key_hex) = group_key_registry
                .key_hex_for(&self.persona_id, &group_id)
                .await
            else {
                // Not resident this session — skip, not an error.
                continue;
            };

            let facts = group_fact_store::load_group_facts_for_context(
                &self.persona_id,
                &group_id,
                &group_key_hex,
            )
            .await
            .map_err(|e| LifecycleError::PersonalStore(e.to_string()))?;

            for fact in facts {
                track
                    .add_group_fact(fact)
                    .map_err(|e| LifecycleError::PersonalStore(e.to_string()))?;
            }
        }

        Ok(())
    }

    /// Apply the decisions.id=424 provenance check to a single entity_facts
    /// row before it enters PersonalTrack. decisions.id=546 exact spec:
    ///
    ///   - same-Persona facts (source_persona_id == this run's persona_id)
    ///     include normally.
    ///   - cross_persona_export=true facts require an explicit per-session
    ///     confirmation before entering assembled context, no persisted
    ///     consent.
    ///   - any fact with mismatched source_persona_id and
    ///     cross_persona_export=false is a hard block, flagged as a system
    ///     integrity error ("correct fact-model behavior should make this
    ///     case unreachable") — not a user-facing privacy warning.
    ///
    /// Cross-Persona confirmation (decisions.id=639, items.id=27): the
    /// confirmation itself happens BEFORE this function ever runs, via a
    /// pre-Focus-start IPC flow entirely outside FocusRun (frontend calls
    /// commands::consent::get_pending_cross_persona_confirmations(), shows
    /// a confirmation UI, then passes the approved fact_ids into
    /// SubmitFocusRunRequest.confirmed_cross_persona_fact_ids, which becomes
    /// self.confirmed_cross_persona_fact_ids). This function only consults
    /// that already-decided set — it does not pause, prompt, or wait.
    ///   - fact.id present in confirmed_cross_persona_fact_ids: user
    ///     approved this specific export this session — include.
    ///   - fact.id absent: either the user declined it, or it was never
    ///     presented (e.g. created between the pre-run query and this
    ///     read — decisions.id=639's documented, accepted race window).
    ///     Either way: OMIT from context, do NOT hard-block the run (Jason,
    ///     items.id=27 session 2026-07-25 — declining one fact should not
    ///     prevent the Focus from running on its other, permitted facts).
    async fn apply_entity_fact_provenance_check(
        &self,
        track: &mut PersonalTrack,
        fact: crate::conductor::types::EntityFact,
    ) -> Result<(), LifecycleError> {
        if fact.source_persona_id == self.persona_id {
            // Same-Persona — include normally.
            track
                .add_entity_fact(fact)
                .map_err(|e| LifecycleError::PersonalStore(e.to_string()))?;
            return Ok(());
        }

        if fact.cross_persona_export {
            if self.confirmed_cross_persona_fact_ids.contains(&fact.id) {
                // User confirmed this export via the pre-Focus-start IPC
                // flow (decisions.id=639) — include.
                track
                    .add_entity_fact(fact)
                    .map_err(|e| LifecycleError::PersonalStore(e.to_string()))?;
                return Ok(());
            }
            // Not in the confirmed set — declined (or not yet presented).
            // Omit from context; do not block the run over one fact.
            //
            // items.id=27 (Jason, 2026-07-25): the omission must be
            // discoverable from the run itself, not only from a log file.
            // Recorded as a disclosure-log entry — the same home gate3 uses
            // for withheld content, and the record R1's privacy audit view
            // (decisions.id=620) is expected to read. gate3's
            // write-before-surface ordering is followed: audit entry first,
            // then the operator-facing warn.
            //
            // RESOLVED (items.id=173, "Layer 8"): this entry was previously
            // discarded because production was wired PrivacyGateway<NoopLogger>,
            // and previously untestable because FocusRun was typed concrete
            // rather than generic over L: DisclosureLogger. Both are now
            // fixed — FocusRun<L: DisclosureLoggerForRun = SqliteDisclosureLogger>
            // persists this entry to the disclosure_log table by default in
            // production, and FocusRun<TestLogger> can inject a TestLogger
            // to assert the entry shape directly. See the module doc
            // comment's "Privacy gateway" section.
            //
            // Recorded on `track` too (decisions.id=815, items.id=27): the
            // frontend's pre-Focus-start query only sees facts pending as of
            // that read, so an omission here still happens on a real race
            // (a fact flips to cross_persona_export=true between that query
            // and this INITIALIZE-phase read) or a plain decline. Read back
            // by FocusRun::emit_status_with_content and carried on
            // RunStatusPayload.provenance_omissions — no separate emit()
            // call site needed here, since every terminal status emission
            // already reads the track fresh.
            if let Err(e) = track.add_omitted_cross_persona_fact(
                crate::conductor::types::OmittedCrossPersonaFact {
                    field_name: fact.field_name.clone(),
                    sensitivity: fact.sensitivity.clone(),
                    origin_persona_id: fact.origin_persona_id.clone(),
                },
            ) {
                log::warn!("lifecycle: failed to record omitted cross-Persona fact on track: {e}");
            }

            if let Some(gateway) = self.privacy_gateway.as_ref() {
                use crate::conductor::privacy::logger::DisclosureLogEntry;
                let entry = DisclosureLogEntry {
                    action_id: "initialize".to_string(),
                    focus_run_id: self.focus_run_id.clone().unwrap_or_default(),
                    // Local context assembly — no external call.
                    execution_tier: 1,
                    abstraction_tier: None,
                    provider: None,
                    fields_shared: vec![],
                    fields_abstracted: IndexMap::new(),
                    // Identifier only, never field_value. entity_id is
                    // Option; empty-string-when-None matches
                    // EntityFact::compute_content_hash()'s existing
                    // convention for the same "entity_id:field_name" key.
                    fields_withheld: vec![format!(
                        "{}:{}",
                        fact.entity_id.as_deref().unwrap_or(""),
                        fact.field_name
                    )],
                    override_declined: false,
                    event_type: "provenance_cross_persona_omitted".to_string(),
                    category: None,
                    fact_key: None,
                };
                // Non-fatal at execution_tier 1, matching the gates' own
                // fatality split. The fact is omitted either way; a failed
                // audit write must not become a second failure on top of it.
                if let Err(e) = gateway.logger.write(entry).await {
                    log::warn!(
                        "lifecycle: disclosure-log write failed for omitted \
                         cross-Persona entity_facts row (id='{}'): {e} — \
                         fact still omitted from context.",
                        fact.id,
                    );
                }
            }

            log::warn!(
                "lifecycle: entity_facts row (id='{}', field='{}', \
                 origin_persona_id={:?}) omitted from context — not present \
                 in confirmed_cross_persona_fact_ids (decisions.id=639).",
                fact.id,
                fact.field_name,
                fact.origin_persona_id,
            );
            return Ok(());
        }

        // source_persona_id != persona_id AND cross_persona_export = false.
        // Per decisions.id=546: "should be unreachable" if the fact model
        // is correct. Hard block, flagged as a system integrity error.
        Err(LifecycleError::ProvenanceIntegrityViolation(format!(
            "entity_facts row (field='{}', source_persona_id='{}') does not \
             belong to this run's persona ('{}') and cross_persona_export is \
             false — decisions.id=546 integrity violation. This case should \
             be unreachable under correct fact-model behavior.",
            fact.field_name, fact.source_persona_id, self.persona_id,
        )))
    }

    /// Load artifact versions for guide IDs declared in this focus's steps.
    /// Non-fatal: returns empty IndexMap on DB error.
    /// Python oracle: FocusRun._load_guide_versions()
    async fn load_guide_versions(&self, guide_ids: &[String]) -> IndexMap<String, String> {
        if guide_ids.is_empty() {
            return IndexMap::new();
        }
        let Ok(mut conn) = self.pool.acquire().await else {
            return IndexMap::new();
        };
        let placeholders = guide_ids.iter().map(|_| "?").collect::<Vec<_>>().join(", ");
        let query_str = format!(
            "SELECT artifact_id, version FROM artifact_versions \
             WHERE artifact_type = 'guide' AND artifact_id IN ({placeholders}) AND revoked = 0"
        );
        let mut q = sqlx::query(&query_str);
        for id in guide_ids {
            q = q.bind(id);
        }
        match q.fetch_all(&mut *conn).await {
            Ok(rows) => rows
                .into_iter()
                .filter_map(|r| {
                    let id: String = r.try_get("artifact_id").ok()?;
                    let ver: String = r.try_get("version").ok()?;
                    Some((id, ver))
                })
                .collect(),
            Err(e) => {
                log::warn!("lifecycle: load_guide_versions failed: {e}");
                IndexMap::new()
            }
        }
    }

    /// Load artifact versions for all active system operators.
    /// Non-fatal: returns empty IndexMap on DB error.
    /// Python oracle: FocusRun._load_operator_versions()
    async fn load_operator_versions(&self) -> IndexMap<String, String> {
        let Ok(mut conn) = self.pool.acquire().await else {
            return IndexMap::new();
        };
        match sqlx::query(
            "SELECT artifact_id, version FROM artifact_versions \
             WHERE artifact_type = 'operator' AND revoked = 0",
        )
        .fetch_all(&mut *conn)
        .await
        {
            Ok(rows) => rows
                .into_iter()
                .filter_map(|r| {
                    let id: String = r.try_get("artifact_id").ok()?;
                    let ver: String = r.try_get("version").ok()?;
                    Some((id, ver))
                })
                .collect(),
            Err(e) => {
                log::warn!("lifecycle: load_operator_versions failed: {e}");
                IndexMap::new()
            }
        }
    }

    // =========================================================================
    // Phase 4 — EXECUTE
    // =========================================================================

    /// items.id=886: seed this pass's store tokens (see conductor::seeding).
    /// Called once at the start of every execute(), so a resumed or re-entered
    /// pass reads the stores again. A Focus that uses no bound name does no I/O.
    /// A failure is a LifecycleError and no step runs.
    async fn seed_store_tokens(&mut self) -> Result<(), LifecycleError> {
        let tokens = {
            let def = self
                .focus_def
                .as_ref()
                .expect("execute() asserted focus_def");
            let row_vars = row_output_vars(&def.steps);
            crate::conductor::seeding::seed_pass(
                &self.user_id,
                &self.persona_id,
                self.key_hex.as_deref().unwrap_or(""),
                self.context_key.as_deref(),
                &self.focus_id,
                &def.steps,
                &row_vars,
            )
            .await?
        };
        self.store_tokens = tokens;
        Ok(())
    }

    /// Run the step loop. Returns Some(RunResult) on early exit (cloud_frontier boundary
    /// or step failure), None when all steps complete normally -> proceed to output().
    /// Python oracle: FocusRun.execute()
    pub async fn execute(&mut self) -> Result<Option<RunResult>, LifecycleError> {
        // Programmer-error guards (not privacy invariants — see D6-348).
        assert!(
            self.personal_track
                .as_ref()
                .map(|t| t.is_sealed())
                .unwrap_or(false),
            "execute() requires a sealed PersonalTrack"
        );
        assert!(self.task_track.is_some(), "execute() requires task_track");
        assert!(
            self.shared_state.is_some(),
            "execute() requires shared_state"
        );
        assert!(self.focus_def.is_some(), "execute() requires focus_def");
        assert!(
            self.focus_run_id.is_some(),
            "execute() requires focus_run_id"
        );
        assert!(
            self.failure_handler.is_some(),
            "execute() requires failure_handler"
        );
        assert!(
            self.privacy_gateway.is_some(),
            "execute() requires privacy_gateway"
        );

        // items.id=886: pass start. Read the bound names this Focus uses from the
        // session and case stores (none: no database is opened). A read that
        // fails fails the pass here, before any step runs.
        self.seed_store_tokens().await?;

        let checkpoint_every = std::env::var("QR_CHECKPOINT_EVERY_N_STEPS")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(3);

        let start = self.current_step;
        let step_count = self.focus_def.as_ref().unwrap().steps.len();
        let mut checkpoint_counter: usize = 0;

        for offset in 0..step_count.saturating_sub(start) {
            let i = start + offset;
            self.current_step = i;

            // items.id=778: the user chose 'Stop and save' for a key rotation.
            if self.stop_requested() {
                return Ok(Some(self.stopped_result()));
            }

            // Clone step to release focus_def borrow before &mut self calls below.
            let step = self.focus_def.as_ref().unwrap().steps[i].clone();

            // items.id=878 (decisions.id=938, G1): a row whose guard is false is
            // SKIPPED, evaluated now over the live track variables. This sits
            // before everything else in the iteration on purpose, so a skipped
            // step emits no status or completion event, writes no ledger row,
            // gate or checkpoint, advances no checkpoint counter and records
            // nothing on the track. Its ordinal is simply passed over. A resume
            // after a checkpointed step re-evaluates the guard on the rebuilt
            // track (rehydrate_focus_run restores every recorded output), so it
            // lands on the same path. YAML-authored steps never carry a guard.
            let clauses = step.guards();
            if !clauses.is_empty() {
                let mut vars = guard::track_vars(self.task_track.as_ref().unwrap());
                // items.id=886: a bound name is a variable from row 0. LOAD
                // rejects an output_var with a bound name, so nothing is
                // overwritten here. An absent session name has no entry.
                vars.extend(self.store_tokens.seeds.clone());
                // items.id=887: and the ids store writers published this pass.
                vars.extend(self.store_vars.clone());
                // items.id=889: every clause must hold (AND). The first one
                // that does not is the one logged.
                if let Some(g) = guard::first_false(&clauses, &vars) {
                    log::debug!(
                        "lifecycle: step '{}' skipped, guard on '{}' is false",
                        step.action_id,
                        g.var
                    );
                    continue;
                }
            }

            // items.id=887: a store writer. Reached only when its guard is true
            // (a skipped row never gets here, so it writes nothing). It makes no
            // model call and emits, ledgers, checkpoints and records NOTHING, like
            // a skipped row, and a failure stops the pass here (fail closed).
            if let Some(op) = &step.store_op {
                self.execute_store_writer(&step, op).await?;
                continue;
            }

            // items.id=129: a block step is internal bookkeeping, not
            // something the user is waiting on. It emits neither a start nor a
            // completion event: its verdict is advisory text and must not
            // reach the chat's live view (ChatPane replaces its live content
            // with every step_content). The step is still recorded on the
            // TaskTrack so later steps can read it by output_var.
            let is_block = step.block_stable_id.is_some();
            if !is_block {
                self.emit_status("running", Some(&step.display_name));
            }

            // items.id=574 piece (c), pulled in by items.id=496: the
            // schedule_trigger gate just below needs a ledger row to set
            // scheduled_for on, and today only reenter_step() writes to
            // focus_run_steps -- this is the ordinary-forward-path insert
            // that piece was still missing. Non-fatal, same tolerance the
            // periodic/Tier-3 checkpoint writes elsewhere in this loop
            // already have -- a ledger-write failure shouldn't abort an
            // otherwise-successful step.
            if let Err(e) = self.ensure_focus_run_step_row(&step.action_id, i).await {
                log::warn!("lifecycle: focus_run_steps ledger write failed: {e}");
            }

            // items.id=496 (Q2): a composition-authored step's one-shot
            // schedule_trigger gates execution eligibility, checked before
            // requires_user_handoff. YAML-authored steps never have one
            // (tokens.rs::ActionDefinition's own doc comment), so this is a
            // no-op for every Focus shipped before this item. A
            // schedule_trigger step on a run with no topic_id is a real
            // authoring error (same Topic-anchor requirement
            // HighPriorityTrigger already has) -- fails loudly here rather
            // than parking forever, since resolve_trigger_anchor() alone
            // has no way to ever resolve an anchor that structurally
            // cannot exist.
            if let Some(trigger) = &step.schedule_trigger {
                if self.topic_id.is_none() {
                    return Err(LifecycleError::ValidationFailed(format!(
                        "step '{}' declares a schedule_trigger but this run has no topic_id",
                        step.action_id
                    )));
                }
                let anchor = self.resolve_trigger_anchor(trigger).await?;
                let now_ts = Utc::now();
                let is_active = anchor.is_some_and(|a| trigger.is_active(a, now_ts));
                if !is_active {
                    // anchor.map(...) when the Topic's anchor field isn't
                    // set yet: scheduled_for = now(), so the sweep re-checks
                    // every tick until the anchor appears, then correctly
                    // transitions to active once trigger.is_active() sees
                    // it -- a self-correcting retry, not a stuck state.
                    let scheduled_for = anchor.map(|a| a + trigger.offset).unwrap_or(now_ts);
                    if let Err(e) = self
                        .mark_focus_run_step_scheduled(&step.action_id, scheduled_for)
                        .await
                    {
                        log::warn!("lifecycle: schedule_trigger ledger write failed: {e}");
                    }
                    if !self._checkpointing_suspended {
                        if let Err(e) = self.write_checkpoint(&step.action_id).await {
                            log::warn!("lifecycle: schedule checkpoint write failed: {e}");
                        }
                    }
                    self.write_focus_run_record_logged("awaiting_schedule")
                        .await;
                    self.emit_status("awaiting_schedule", Some(&step.display_name));
                    return Ok(Some(RunResult {
                        focus_run_id: self.focus_run_id.clone().unwrap_or_default(),
                        status: "awaiting_schedule".to_owned(),
                        output_id: None,
                        output_content: None,
                        failure: None,
                        crisis_resource_block: None,
                    }));
                }
            }

            // Handoff-pause boundary (items.id=439, Part 6d — fully decoupled
            // from external_access, formerly step.routing_tier == 3):
            // checkpoint (if not suspended), set status, return early.
            if step.requires_user_handoff {
                if !self._checkpointing_suspended {
                    if let Err(e) = self.write_checkpoint(&step.action_id).await {
                        log::warn!("lifecycle: Tier 3 checkpoint write failed: {e}");
                    }
                }
                self.write_focus_run_record_logged("awaiting_user").await;
                // R1 crisis-handling floor (items.id=297): cloud_frontier is a pause
                // before OUTPUT, so without this the crisis-flagged run's
                // resource block would never surface -- see crisis.rs module
                // doc for the scope boundary this closes.
                let crisis_resource_block = if self.crisis_floor_triggered {
                    Some(crate::conductor::crisis::resource_block())
                } else {
                    None
                };
                // items.id=317: the draft awaiting Gate3 review is whatever
                // the last completed step produced -- OUTPUT is never
                // reached from this pause, so without capturing it here it's
                // lost and the assistant message's content stays empty
                // forever (send_message's backfill only pulls from
                // output_store, which this run never writes to).
                let draft_content = self
                    .task_track
                    .as_ref()
                    .and_then(|tt| tt.last_output())
                    .map(|s| s.to_owned());
                self.emit_status_with_content(
                    "awaiting_user",
                    Some(&step.display_name),
                    draft_content.as_deref(),
                    crisis_resource_block.as_deref(),
                );
                return Ok(Some(RunResult {
                    focus_run_id: self.focus_run_id.clone().unwrap_or_default(),
                    status: "awaiting_user".to_owned(),
                    output_id: None,
                    output_content: draft_content,
                    failure: None,
                    crisis_resource_block,
                }));
            }

            let step_failure = self.execute_step(&step, i).await?;

            // items.id=778: a step that failed because it was stopped is not a
            // failure to park or retry; the run is cancelled. items.id=816: a
            // user's Cancel and an API Chat Stop during a retry wait end here
            // too.
            if step_failure
                .as_ref()
                .is_some_and(|f| self.failure_ends_run(f))
            {
                return Ok(Some(self.stopped_result()));
            }

            if let Some(failure) = step_failure {
                if failure.action == FailureAction::Degrade {
                    // F8: suspend checkpointing and continue with next step.
                    self._checkpointing_suspended = true;
                    continue;
                }
                let result = self.handle_step_failure(failure).await?;
                return Ok(Some(result));
            }

            // Step completed successfully -- emit its real content for
            // ChatPane.tsx's staged/incremental reveal (items.id=245-ish),
            // distinct from the step-START announcement above (which fires
            // before content exists). last_output() is the step just pushed
            // by execute_step() above.
            if !is_block {
                let step_content = self
                    .task_track
                    .as_ref()
                    .and_then(|tt| tt.last_output())
                    .map(|s| s.to_owned());
                self.emit_step_complete(&step.display_name, step_content.as_deref());
            }

            checkpoint_counter += 1;
            if checkpoint_counter >= checkpoint_every && !self._checkpointing_suspended {
                if let Err(e) = self.write_checkpoint(&step.action_id).await {
                    log::warn!("lifecycle: periodic checkpoint write failed: {e}");
                }
                checkpoint_counter = 0;
            }
        }

        Ok(None) // All steps completed — proceed to Phase 5 OUTPUT
    }

    /// Give this run its writer lease and bind the lease to the run's id, so a
    /// user's cancel can find the run (items.id=816, `cancel_run`). The id must
    /// already be set (by `authorize` or `rehydrate_focus_run`, which every
    /// caller has run): without it nothing could be bound, so this fails and
    /// attaches nothing, and the lease is released as it drops, rather than
    /// leave a run that Cancel cannot reach.
    pub(crate) fn attach_lease(
        &mut self,
        lease: crate::auth::writer_gate::WriterLease,
    ) -> Result<(), LifecycleError> {
        let Some(run_id) = self.focus_run_id.as_deref() else {
            return Err(LifecycleError::ValidationFailed(
                "a run's id must be set before its writer lease is attached".to_owned(),
            ));
        };
        lease.bind_run(run_id);
        self.lease = Some(lease);
        Ok(())
    }

    /// Whether the user chose to stop this run for a key rotation
    /// (items.id=778) or cancelled it (items.id=816): its lease's stop token
    /// has been tripped.
    pub(crate) fn stop_requested(&self) -> bool {
        self.lease
            .as_ref()
            .is_some_and(|l| l.stop_token().is_cancelled())
    }

    /// Whether a failed step ends this run as cancelled (items.id=778, 816):
    /// the lease's stop token is tripped, or the step itself reports a user
    /// Stop (an API Chat Stop during a retry wait trips only the stream's
    /// child token, so the lease token is not tripped). `USER_STOP` is built
    /// only by `stopped_failure` in executor.rs.
    fn failure_ends_run(&self, failure: &FailureResult) -> bool {
        self.stop_requested() || is_user_stop(failure)
    }

    /// The result of a run stopped for a key rotation: `cancelled`, with no
    /// failure to show. `execute_and_finish` hands this status to `cleanup`,
    /// which writes `cancelled` to focus_runs and purges the snapshots. A
    /// streamed reply's partial text was already kept by the executor.
    fn stopped_result(&self) -> RunResult {
        self.emit_status("cancelled", None);
        RunResult {
            focus_run_id: self.focus_run_id.clone().unwrap_or_default(),
            status: "cancelled".to_owned(),
            output_id: None,
            output_content: None,
            failure: None,
            crisis_resource_block: None,
        }
    }

    /// items.id=887: run one store-writer row (decisions.id=939 section 4). The
    /// handler (conductor::store_writer) calls only the typed, latch-gated store
    /// functions with the running focus_id and context_key. Arguments are read
    /// from the pass's variables: the track's outputs, the ids earlier writers
    /// published, and the seeded session names. A failure of any kind is an Err
    /// and ends the pass; a one-time chat's refusal is Ok and writes nothing.
    async fn execute_store_writer(
        &mut self,
        step: &ActionDefinition,
        op: &store_op::StoreOp,
    ) -> Result<(), LifecycleError> {
        use crate::conductor::store_writer::{run_store_op, StoreWriteError, WriterContext};

        let run_id = self.focus_run_id.clone().unwrap_or_default();
        let mut vars = guard::track_vars(self.task_track.as_ref().ok_or_else(|| {
            LifecycleError::ValidationFailed(format!(
                "step '{}': store writer run without a task track",
                step.action_id
            ))
        })?);
        vars.extend(self.store_vars.clone());
        vars.extend(self.store_tokens.seeds.clone());

        let mut conn = crate::persistence::output_store::open_outputs_db(
            &self.user_id,
            &self.persona_id,
            self.key_hex.as_deref().unwrap_or(""),
        )
        .await
        .map_err(|e| StoreWriteError::Open(e.to_string()))?;
        let ctx = WriterContext {
            context_key: self.context_key.as_deref(),
            focus_id: &self.focus_id,
            run_id: &run_id,
            action_id: &step.action_id,
        };
        let done = run_store_op(&mut conn, &ctx, op, &vars).await?;
        log::debug!(
            "lifecycle: store writer '{}' ({}) -> {:?}",
            step.action_id,
            op.op.as_str(),
            done.outcome
        );
        if let Some((var, id)) = done.published {
            self.store_vars.insert(var, id);
        }
        Ok(())
    }

    /// items.id=129: run a composition-authored block step. Pure over the
    /// TaskTrack: no provider call, no PersonalTrack, no tier computation
    /// (a block step is LocalOnly by construction, see
    /// load_focus_definition_from_db()). The handler's output is recorded with
    /// TaskTrack::add_step, the only writer, so later steps can read it by
    /// output_var.
    ///
    /// A missing handler or customization here means a FocusDefinition was
    /// built without going through the loader (LOAD checks both). That is
    /// reported as a ValidationFailed error rather than assumed away.
    fn execute_block_step(
        &mut self,
        step: &ActionDefinition,
        block_id: &str,
    ) -> Result<Option<FailureResult>, LifecycleError> {
        let handler =
            crate::conductor::block_dispatch::block_handler(block_id).ok_or_else(|| {
                LifecycleError::ValidationFailed(format!(
                    "step '{}': block '{block_id}' has no compiled-in handler (the loader checks \
                     this at LOAD, so this FocusDefinition did not come from it)",
                    step.action_id
                ))
            })?;
        let customization = step.block_customization.as_ref().ok_or_else(|| {
            LifecycleError::ValidationFailed(format!(
                "step '{}': block '{block_id}' has no customization payload",
                step.action_id
            ))
        })?;
        let task_track = self.task_track.as_mut().ok_or_else(|| {
            LifecycleError::ValidationFailed(format!(
                "step '{}': block step run without a task track",
                step.action_id
            ))
        })?;

        let out = (handler.run)(customization, task_track).map_err(|e| {
            LifecycleError::ValidationFailed(format!(
                "step '{}': block '{block_id}': {e}",
                step.action_id
            ))
        })?;

        task_track.add_step(TaskStep {
            action_id: step.action_id.clone(),
            output_var: Some(out.output_var),
            content: out.content,
            // 1 = general: a block step never raises the run's sensitivity
            // ceiling (add_step keeps the max).
            sensitivity_severity: 1,
            // Local, no provider.
            routing_tier_used: 1,
            personal_flagged: false,
            audit_unavailable: false,
        });
        Ok(None)
    }

    /// Compute tier values (ADR-012 Amendment 3); construct StepContext;
    /// read floor_consent_preference; delegate to StepExecutor.
    ///
    /// Rust deviation from Python oracle:
    ///   StepContext holds step metadata only. PersonalTrack, TaskTrack,
    ///   SharedStateTrack, FailureHandler, PrivacyGateway, and ConductorScheduler
    ///   are borrowed from distinct FocusRun fields and passed separately to
    ///   StepExecutor::execute(). This satisfies D6-342 (no Arc<Mutex<>>) while
    ///   working within Rust's borrow checker constraints.
    ///
    /// Python oracle: FocusRun._execute_step()
    async fn execute_step(
        &mut self,
        step: &ActionDefinition,
        step_index: usize,
    ) -> Result<Option<FailureResult>, LifecycleError> {
        // items.id=496 (judgment calls 5/6), items.id=129: block_stable_id
        // dispatch. None is the unchanged path every YAML-authored step (and
        // a NULL-block composition row) takes. Some(id) runs the compiled-in
        // handler from conductor::block_dispatch::block_handler() -- the same
        // registry load_focus_definition_from_db() checks at LOAD -- and
        // records the result on the TaskTrack. A block step makes no provider
        // call and never reaches StepExecutor.
        if let Some(block_id) = &step.block_stable_id {
            return self.execute_block_step(step, block_id);
        }

        // Axis 1: execution_tier — min(focus_max_permitted, focus_max_routing, step.routing_tier).
        // items.id=529: computed via ExternalAccess::min() (correct regardless
        // of any numbering) rather than raw u8::min() (which depended on the
        // legacy numbers' ordering matching ExternalAccess's real ordering —
        // a dependency that would have made AnonymousPreferred unsafe to
        // store numerically; see shared_020.sql's header comment). Bridged
        // back to u8 via execution_tier_ordinal() for this axis's own
        // downstream consumers, left untouched (items.id=528 territory).
        let execution_tier = {
            let fd = self.focus_def.as_ref().unwrap();
            execution_tier_ordinal(ExternalAccess::min(
                ExternalAccess::min(self._focus_max_permitted_tier, fd.max_routing_tier),
                step.routing_tier,
            ))
        };

        // Axis 2 input: this step's actual effective access, folding in its own
        // override — distinct from focus_external_access (Focus-level-only,
        // items.id=439), which the abstraction floor cannot correctly key off
        // alone (PROVIDER_REGISTRY_AND_TIER_MODEL_SPEC.md Part 7a).
        let effective_access = step
            .external_access_override
            .map(|o| o.min(self._focus_external_access))
            .unwrap_or(self._focus_external_access);

        // Axis 2: abstraction_tier with floor clamping (ADR-012 Amendment 3)
        let raw_abstraction = self._focus_privacy_tier.min(execution_tier);
        let abstraction_tier = if effective_access != ExternalAccess::LocalOnly {
            raw_abstraction.max(2) // floor clamp: abstraction_tier >= 2 when external access is live (items.id=444, Part 7)
        } else {
            raw_abstraction
        };

        log::debug!(
            "lifecycle: step={} execution_tier={} effective_access={} abstraction_tier={} \
             raw_abstraction={} focus_privacy_tier={}",
            step.action_id,
            execution_tier,
            effective_access.as_str(),
            abstraction_tier,
            raw_abstraction,
            self._focus_privacy_tier,
        );

        // Gate3 look-ahead: next step's execution_tier (see next_execution_tier()).
        let next_execution_tier: Option<u8> = {
            let fd = self.focus_def.as_ref().unwrap();
            next_execution_tier(
                &fd.steps,
                step_index,
                ExternalAccess::min(self._focus_max_permitted_tier, fd.max_routing_tier),
            )
        };

        // Clone values needed in the async block / StepContext before any &mut borrows.
        // Avoids holding references to self fields across the async block boundary.
        let persona_id = self.persona_id.clone();
        let focus_id = self.focus_id.clone();
        let focus_run_id = self.focus_run_id.clone().unwrap_or_default();
        let user_input = self.user_input.clone();
        let persona_context = self._persona_context_rendered.clone();
        let focus_external_access = self._focus_external_access;
        let scheduler = Arc::clone(&self.scheduler);
        let user_id = self.user_id.clone();

        // Floor consent preference (D5-152). Read from the Persona's encrypted
        // personal.db (items.id=661). Non-fatal -- the consent gate fires
        // normally if the read fails, or if the run has no key any more.
        let floor_consent_preference: Option<String> = async {
            let key_hex = self.key_hex.as_deref()?;
            let consent = crate::persistence::personal_store::read_floor_consent_preference(
                &user_id,
                &persona_id,
                key_hex,
            )
            .await
            .ok()??;
            match consent.mode.as_str() {
                "modified" => {
                    let stored_tier = consent.abstraction_tier? as u8;
                    (stored_tier <= abstraction_tier).then(|| "modified".to_owned())
                }
                "local" => Some("local".to_owned()),
                _ => None,
            }
        }
        .await;

        // Hosted-provider resolution (items.id=251/432, ceiling-aware since
        // items.id=692). Only relevant when the step's effective_access
        // permits external execution -- a LocalOnly step never dispatches to
        // a hosted provider. The candidate set is
        // providers.provider_type='cloud_inference_api' (items.id=430),
        // narrowed by the step's ceiling (anonymous_required keeps only
        // is_anonymous providers; anonymous_preferred ranks them first), then
        // resolved via user_provider_preference_store's Focus -> Persona ->
        // account precedence (items.id=428). An explicit user preference wins
        // among permitted providers. Three outcomes, see
        // provider_selection::resolve_external_provider: a selected provider;
        // none set up (or ambiguous, or a DB read failure) -> StepExecutor
        // raises the F10 MissingQrHostedConfig failure rather than guessing;
        // or the user's provider is excluded by the ceiling -> StepExecutor
        // raises ProviderExcludedByCeiling, naming the setting to change.
        // Gated on effective_access (the step's own typed capability ceiling,
        // computed above) rather than the execution_tier numeric bridge --
        // see StepContext::effective_access's own doc comment for why this
        // file keeps computing both.
        let (qr_hosted_provider_preference, qr_hosted_ceiling_excluded): (
            Option<String>,
            Option<String>,
        ) = if effective_access != ExternalAccess::LocalOnly {
            use crate::conductor::provider_selection::{
                resolve_external_provider, ExternalProviderResolution,
            };
            let resolution = match &self.chat_binding {
                // items.id=716: a bound conversation resolves its own
                // provider; the user's preference is not consulted. The
                // ceiling still applies.
                Some(bound) => {
                    crate::conductor::provider_selection::resolve_bound_provider(
                        &self.pool,
                        &bound.provider_id,
                        effective_access,
                    )
                    .await
                }
                None => {
                    resolve_external_provider(
                        &self.pool,
                        &user_id,
                        self.key_hex.as_deref().unwrap_or(""),
                        &persona_id,
                        &focus_id,
                        effective_access,
                    )
                    .await
                }
            };
            match resolution {
                ExternalProviderResolution::Selected(id) => (Some(id), None),
                ExternalProviderResolution::NotConfigured => (None, None),
                ExternalProviderResolution::ExcludedByCeiling { provider_name } => {
                    (None, Some(provider_name))
                }
            }
        } else {
            (None, None)
        };

        let ctx = StepContext {
            step: step.clone(),
            focus_id,
            focus_run_id,
            user_input,
            persona_context,
            focus_external_access,
            effective_access,
            execution_tier,
            abstraction_tier,
            raw_abstraction,
            floor_consent_preference,
            qr_hosted_provider_preference,
            qr_hosted_ceiling_excluded,
            next_execution_tier,
            retry_count: 0,
            focus_name: self
                .focus_def
                .as_ref()
                .map(|d| d.display_name.clone())
                .unwrap_or_default(),
            user_id: self.user_id.clone(),
            persona_id: self.persona_id.clone(),
            key_hex: self.key_hex.clone().unwrap_or_default(),
            attendance: self.attendance,
            stream: self.chat_stream.clone(),
            stop: self.lease.as_ref().map(|l| l.stop_token().clone()),
            bound: self.chat_binding.clone(),
            store_tokens: self.store_tokens.clone(),
        };

        // Borrow distinct fields of self simultaneously — Rust borrow checker allows
        // split field borrows within a single &mut self context.
        let personal_track = self.personal_track.as_ref().unwrap();
        let task_track = self.task_track.as_mut().unwrap();
        let shared_state = self.shared_state.as_mut().unwrap();
        let failure_handler = self.failure_handler.as_ref().unwrap();
        let privacy_gateway = self.privacy_gateway.as_ref().unwrap();

        Ok(StepExecutor::new()
            .execute(
                ctx,
                personal_track,
                task_track,
                shared_state,
                failure_handler,
                privacy_gateway,
                &scheduler,
                self.app_handle.as_ref(),
                &self.pool,
            )
            .await)
    }

    /// Update run status based on failure action; build RunResult.
    /// Python oracle: FocusRun._handle_step_failure()
    async fn handle_step_failure(
        &mut self,
        failure: FailureResult,
    ) -> Result<RunResult, LifecycleError> {
        // R1 crisis-handling floor (items.id=297): every exit this function
        // can take (Stop/failed, and AwaitUser/OfferTier2/
        // OfferCompact/AwaitFloorConsent/AwaitConsent -- cloud_frontier, consent
        // gates, and Gate3-hold all funnel through here) is a pause/failure
        // before OUTPUT, so without this the crisis-flagged run's resource
        // block would never surface -- see crisis.rs module doc.
        let crisis_resource_block = if self.crisis_floor_triggered {
            Some(crate::conductor::crisis::resource_block())
        } else {
            None
        };

        if failure.action == FailureAction::Stop && !failure.is_recoverable {
            self.write_focus_run_record_logged("failed").await;
            self.emit_status_with_content("failed", None, None, crisis_resource_block.as_deref());
        } else if matches!(
            failure.action,
            FailureAction::AwaitUser
                | FailureAction::OfferTier2
                | FailureAction::OfferCompact
                | FailureAction::AwaitFloorConsent
                | FailureAction::AwaitConsent
        ) {
            // Checkpoint the paused step before parking (items.id=245): unlike
            // execute()'s Tier 3 requires_user_handoff branch, which already
            // checkpoints itself before pausing, this generic consent-gate
            // pause path previously had no checkpoint at all. rehydrate_focus_run()'s
            // status=='awaiting_user' branch resumes AT the last-checkpointed
            // step, so without this write a resume would land on a stale
            // periodic checkpoint (or step 0, if none exists yet) instead of
            // the step that actually paused. failure.action_id is always Some()
            // on this path: every real call site threads Some(&ctx.step.action_id)
            // into failure_handler.handle() (see executor.rs/failure.rs).
            if !self._checkpointing_suspended {
                if let Some(action_id) = failure.action_id.clone() {
                    if let Err(e) = self.write_checkpoint(&action_id).await {
                        log::warn!("lifecycle: consent-gate pause checkpoint write failed: {e}");
                    }
                }
            }
            self.write_focus_run_record_logged("awaiting_user").await;
            self.emit_status_with_content(
                "awaiting_user",
                None,
                None,
                crisis_resource_block.as_deref(),
            );
        }

        let status = self.get_current_status().await;
        Ok(RunResult {
            focus_run_id: self.focus_run_id.clone().unwrap_or_default(),
            status,
            output_id: None,
            output_content: None,
            failure: Some(failure),
            crisis_resource_block,
        })
    }

    /// Read current status from focus_runs table. Returns "unknown" on error.
    /// Python oracle: FocusRun._get_current_status()
    async fn get_current_status(&self) -> String {
        let focus_run_id = self.focus_run_id.as_deref().unwrap_or("");
        let key_hex = self.key_hex.as_deref().unwrap_or("");
        if key_hex.is_empty() {
            return "unknown".to_owned();
        }
        let Ok(mut conn) = open_outputs_db(&self.user_id, &self.persona_id, key_hex).await else {
            return "unknown".to_owned();
        };
        sqlx::query("SELECT status FROM focus_runs WHERE id = ?")
            .bind(focus_run_id)
            .fetch_optional(&mut conn)
            .await
            .ok()
            .flatten()
            .and_then(|r| r.try_get::<String, _>("status").ok())
            .unwrap_or_else(|| "unknown".to_owned())
    }

    /// Write a checkpoint snapshot to focus_run_snapshots.
    /// PersonalTrack is never serialized — only the manifest (field names + hashes).
    /// SHA-256 of (task_json + shared_json + manifest_json) forms the integrity hash.
    /// Python oracle: FocusRun._write_checkpoint()
    async fn write_checkpoint(&self, action_id: &str) -> Result<(), LifecycleError> {
        let personal_track = self.personal_track.as_ref().unwrap();
        let task_track = self.task_track.as_ref().unwrap();
        let shared_state = self.shared_state.as_ref().unwrap();
        let focus_run_id = self.focus_run_id.as_deref().unwrap();
        let key_hex = self.key_hex.as_deref().unwrap();

        // Task track JSON — matches Python oracle dict structure
        let task_data = serde_json::json!({
            "steps": task_track.steps().iter().map(|s| serde_json::json!({
                "action_id": s.action_id,
                "output_var": s.output_var,
                "content": s.content,
                "sensitivity_severity": s.sensitivity_severity,
                "routing_tier_used": s.routing_tier_used,
                "personal_flagged": s.personal_flagged,
                "audit_unavailable": s.audit_unavailable,
            })).collect::<Vec<_>>(),
            "sensitivity_ceiling": task_track.sensitivity_ceiling(),
        });

        // Shared state JSON — matches Python oracle dict structure
        let shared_data = serde_json::json!({
            "step_disclosure_buffers": shared_state.buffers(),
            "promotions": shared_state.promotions().iter().map(|p| serde_json::json!({
                "action_id": p.action_id,
                "content_key": p.content_key,
                "content": p.content,
            })).collect::<Vec<_>>(),
        });

        // Personal context manifest (field names + hashes only — never raw values)
        let manifest = PersonalContextManifest::from_personal_track(personal_track, now());
        let manifest_data = serde_json::json!({
            "field_names": manifest.field_names,
            "field_hashes": manifest.field_hashes,
            "source_versions": manifest.source_versions,
            "snapshot_taken_at": manifest.snapshot_taken_at,
        });

        let task_json = serde_json::to_string(&task_data)?;
        let shared_json = serde_json::to_string(&shared_data)?;
        let manifest_json = serde_json::to_string(&manifest_data)?;

        // SHA-256 integrity hash — matches Python oracle: hashlib.sha256(...)
        let combined = format!("{task_json}{shared_json}{manifest_json}");
        let mut hasher = Sha256::new();
        hasher.update(combined.as_bytes());
        let checkpoint_hash = format!("{:x}", hasher.finalize());

        let mut conn = open_outputs_db(&self.user_id, &self.persona_id, key_hex).await?;

        sqlx::query(
            "INSERT INTO focus_run_snapshots
             (id, focus_run_id, action_id, phase, task_track_json,
              shared_state_json, personal_context_manifest,
              checkpoint_hash, created_at)
             VALUES (?, ?, ?, 4, ?, ?, ?, ?, ?)",
        )
        .bind(Uuid::new_v4().to_string())
        .bind(focus_run_id)
        .bind(action_id)
        .bind(&task_json)
        .bind(&shared_json)
        .bind(&manifest_json)
        .bind(&checkpoint_hash)
        .bind(now())
        .execute(&mut conn)
        .await?;

        Ok(())
    }

    /// items.id=574 piece (c), pulled in as a prerequisite by items.id=496's
    /// schedule_trigger gate (execute(), below): the ordinary forward
    /// execute() loop had no insert path into focus_run_steps at all before
    /// this -- only reenter_step() wrote to this table. Ensures a 'pending'
    /// live row exists for (focus_run_id, action_id) at attempt 1; no-op if a
    /// live row already exists (idx_focus_run_steps_live) -- e.g. a
    /// rehydrated/resumed run re-entering a step it already has a row for.
    /// Does not attempt items.id=574's other remaining pieces (input_vars
    /// wiring, piece (b)) -- out of scope here.
    async fn ensure_focus_run_step_row(
        &self,
        action_id: &str,
        sequence_index: usize,
    ) -> Result<(), LifecycleError> {
        let focus_run_id = self.focus_run_id.as_deref().unwrap();
        let key_hex = self.key_hex.as_deref().ok_or(LifecycleError::NoKey)?;
        let mut conn = open_outputs_db(&self.user_id, &self.persona_id, key_hex).await?;
        let timestamp = now();

        let exists: Option<i64> = sqlx::query_scalar(
            "SELECT 1 FROM focus_run_steps
             WHERE focus_run_id = ? AND action_id = ? AND status != 'discarded'
             LIMIT 1",
        )
        .bind(focus_run_id)
        .bind(action_id)
        .fetch_optional(&mut conn)
        .await?;
        if exists.is_some() {
            return Ok(());
        }

        sqlx::query(
            "INSERT INTO focus_run_steps
             (id, focus_run_id, action_id, attempt, sequence_index, status, created_at, updated_at)
             VALUES (?, ?, ?, 1, ?, 'pending', ?, ?)",
        )
        .bind(Uuid::new_v4().to_string())
        .bind(focus_run_id)
        .bind(action_id)
        .bind(sequence_index as i64)
        .bind(&timestamp)
        .bind(&timestamp)
        .execute(&mut conn)
        .await?;

        Ok(())
    }

    /// items.id=496: sets the live focus_run_steps row's scheduled_for for
    /// a step whose schedule_trigger has not yet fired --
    /// conductor::scheduled_sweep queries this column to know when to nudge
    /// resume_run. Assumes ensure_focus_run_step_row() already ran for this
    /// step this call (execute()'s loop always calls it first).
    async fn mark_focus_run_step_scheduled(
        &self,
        action_id: &str,
        scheduled_for: DateTime<Utc>,
    ) -> Result<(), LifecycleError> {
        let focus_run_id = self.focus_run_id.as_deref().unwrap();
        let key_hex = self.key_hex.as_deref().ok_or(LifecycleError::NoKey)?;
        let mut conn = open_outputs_db(&self.user_id, &self.persona_id, key_hex).await?;

        sqlx::query(
            "UPDATE focus_run_steps SET scheduled_for = ?, updated_at = ?
             WHERE focus_run_id = ? AND action_id = ? AND status != 'discarded'",
        )
        .bind(scheduled_for.to_rfc3339())
        .bind(now())
        .bind(focus_run_id)
        .bind(action_id)
        .execute(&mut conn)
        .await?;

        Ok(())
    }

    /// items.id=496 (Q2): resolves a schedule_trigger's anchor timestamp
    /// from this run's Topic (self.topic_id) -- same extra_metadata
    /// [anchor_field] lookup commands::active_board::topic_is_high_priority
    /// already makes for the Focus-level HighPriorityTrigger, reused here
    /// for the per-step schedule_trigger. Callers must check
    /// self.topic_id.is_some() themselves first (execute()'s gate treats a
    /// schedule_trigger step on a topic-less run as a hard LOAD-shaped
    /// error, not a silent forever-wait) -- this fn only handles the
    /// "topic exists but the anchor field isn't set on it yet" case,
    /// failing closed (None, not Err) since that's a legitimate transient
    /// state a later Topic update can resolve.
    async fn resolve_trigger_anchor(
        &self,
        trigger: &HighPriorityTrigger,
    ) -> Result<Option<DateTime<Utc>>, LifecycleError> {
        let Some(topic_id) = self.topic_id.as_deref() else {
            return Ok(None);
        };
        let Some(key_hex) = self.key_hex.as_deref() else {
            return Ok(None);
        };
        let topic = crate::persistence::topic_store::get_topic(
            &self.user_id,
            &self.persona_id,
            key_hex,
            topic_id,
        )
        .await
        .map_err(|e| LifecycleError::TopicStore(e.to_string()))?;
        let Some(topic) = topic else {
            return Ok(None);
        };
        let anchor_str = topic
            .extra_metadata
            .get(&trigger.anchor_field)
            .and_then(|v| v.as_str());
        let Some(anchor_str) = anchor_str else {
            return Ok(None);
        };
        Ok(DateTime::parse_from_rfc3339(anchor_str)
            .ok()
            .map(|dt| dt.with_timezone(&Utc)))
    }

    // =========================================================================
    // Phase 5 — OUTPUT
    // =========================================================================

    /// Save final output; purge snapshots; write run_history; set awaiting_feedback.
    /// Python oracle: FocusRun.output()
    pub async fn output(&mut self) -> Result<RunResult, LifecycleError> {
        use crate::persistence::output_store::save_output;

        let mut final_content = self
            .task_track
            .as_ref()
            .unwrap()
            .last_output()
            .unwrap_or("")
            .to_owned();

        // R1 crisis-handling floor (decisions.id=607, items.id=265): append
        // (never replace) the model's real response with the static local
        // resource block. This is the run's success-path content-finalization
        // point, reached identically regardless of Persona/Focus/tier -- see
        // crisis.rs module doc for the full reasoning. cloud_frontier pauses and step
        // failures don't reach here at all (they return early from execute());
        // items.id=297 covers those via the cloud_frontier boundary and
        // handle_step_failure() instead, each with its own identical check.
        let crisis_resource_block = if self.crisis_floor_triggered {
            Some(crate::conductor::crisis::resource_block())
        } else {
            None
        };
        if let Some(block) = &crisis_resource_block {
            final_content.push_str("\n\n");
            final_content.push_str(block);
        }

        let output_type = self.focus_def.as_ref().unwrap().output_type.clone();
        let focus_run_id = self.focus_run_id.clone().unwrap_or_default();
        let key_hex = self.key_hex.clone().unwrap_or_default();
        let sensitivity = self.output_sensitivity().to_owned();
        let output_id = Uuid::new_v4().to_string();

        // items.id=416 (decisions.id=767): Privacy Guardian classification
        // established once, at creation, rather than deferred to whenever a
        // later action (e.g. a Library copy) first triggers it -- outputs
        // are immutable after this point (output_store.rs's own header:
        // the only later mutation is delete-time content-zeroing), so a
        // scan computed here can never go stale. execution_tier uses
        // _focus_max_permitted_tier (the run's hard ceiling) since no single
        // per-run "final" execution tier is tracked -- it's computed fresh
        // per-step (see execute_step's own Axis-1 calculation), not
        // persisted on FocusRun.
        let scan_result = scan_output(
            &self.privacy_gateway.as_ref().unwrap().logger,
            "output-creation",
            &focus_run_id,
            &final_content,
            execution_tier_ordinal(self._focus_max_permitted_tier),
            sensitivity_severity(&sensitivity),
            ScanIntensity::Full,
        )
        .await
        .map_err(|e| LifecycleError::PrivacyScan(e.to_string()))?;

        save_output(
            &self.user_id,
            &self.persona_id,
            &key_hex,
            &focus_run_id,
            &output_type,
            &final_content,
            &sensitivity,
            Some(&output_id),
            Some(&scan_result),
        )
        .await
        .map_err(|e| LifecycleError::OutputStore(e.to_string()))?;

        self._output_id = Some(output_id.clone());
        self.purge_snapshots().await;
        self.write_run_history(&output_id, &output_type).await;
        self.write_focus_run_record("awaiting_feedback").await?;
        self.emit_status("awaiting_feedback", None);

        Ok(RunResult {
            focus_run_id,
            status: "awaiting_feedback".to_owned(),
            output_id: Some(output_id),
            output_content: Some(final_content),
            failure: None,
            crisis_resource_block,
        })
    }

    /// Map sensitivity_ceiling integer to canonical string.
    /// Python oracle: FocusRun._output_sensitivity()
    fn output_sensitivity(&self) -> &'static str {
        let ceiling = self
            .task_track
            .as_ref()
            .map(|tt| tt.sensitivity_ceiling())
            .unwrap_or(1);
        match ceiling {
            1 => "general",
            2 => "personal",
            3 => "medical",
            4 => "financial",
            _ => "general",
        }
    }

    /// Delete all snapshots for this run. Non-fatal.
    /// Python oracle: FocusRun._purge_snapshots()
    async fn purge_snapshots(&self) {
        let Some(focus_run_id) = &self.focus_run_id else {
            return;
        };
        let key_hex = self.key_hex.as_deref().unwrap_or("");
        if key_hex.is_empty() {
            return;
        }
        let Ok(mut conn) = open_outputs_db(&self.user_id, &self.persona_id, key_hex).await else {
            return;
        };
        let _ = sqlx::query("DELETE FROM focus_run_snapshots WHERE focus_run_id = ?")
            .bind(focus_run_id)
            .execute(&mut conn)
            .await;
    }

    /// Write run_history discovery entry after output is saved. Non-fatal.
    /// Python oracle: FocusRun._write_run_history()
    async fn write_run_history(&self, output_id: &str, output_type: &str) {
        use crate::persistence::topic_store::create_run_history_entry;
        let key_hex = self.key_hex.as_deref().unwrap_or("");
        let focus_run_id = self.focus_run_id.as_deref().unwrap_or("");
        if let Err(e) = create_run_history_entry(
            &self.user_id,
            &self.persona_id,
            key_hex,
            focus_run_id,
            &self.focus_id,
            self.is_quick_ask,
            self.topic_id.as_deref(),
            Some(output_id),
            Some(output_type),
        )
        .await
        {
            log::warn!(
                "lifecycle: run_history write failed (non-fatal) — \
                 focus={} focus_run_id={} error={e}",
                self.focus_id,
                focus_run_id,
            );
        }
    }

    // =========================================================================
    // Phase 7 — CLEANUP
    // =========================================================================

    /// Drop tracks; purge snapshots (if terminal); update final run status.
    /// Python oracle: FocusRun.cleanup()
    pub async fn cleanup(&mut self, final_status: &str) {
        self.personal_track = None;
        self.task_track = None;
        self.shared_state = None;
        self.privacy_gateway = None;
        self._persona_context_rendered.clear();

        if matches!(final_status, "complete" | "cancelled" | "failed") {
            self.purge_snapshots().await;
        }
        self.write_focus_run_record_logged(final_status).await;
        self.emit_status(final_status, None);
    }

    // =========================================================================
    // Convenience: execute_full
    // =========================================================================

    /// Run all seven phases sequentially. Returns RunResult on any outcome.
    ///
    /// F_SYSTEM errors (TaxonomyIntegrity, DatabaseMigration) are caught here
    /// and returned as structured FailureResult inside Ok(RunResult{status:"failed"}).
    /// All other errors propagate as Err(LifecycleError).
    ///
    /// Python oracle: FocusRun.execute_full()
    pub async fn execute_full(&mut self) -> Result<RunResult, LifecycleError> {
        match self.execute_full_inner().await {
            Ok(result) => Ok(result),
            Err(e) => {
                // F_SYSTEM: TaxonomyIntegrity or DatabaseMigration -> FailureResult
                let maybe_conductor = match &e {
                    LifecycleError::TaxonomyIntegrity(msg) => {
                        Some(ConductorError::TaxonomyIntegrity {
                            plain_language: msg.clone(),
                        })
                    }
                    LifecycleError::DatabaseMigration(msg) => {
                        Some(ConductorError::DatabaseMigration {
                            plain_language: msg.clone(),
                        })
                    }
                    _ => None,
                };

                if let Some(conductor_err) = maybe_conductor {
                    let failure = if let Some(fh) = &self.failure_handler {
                        fh.handle(&conductor_err, None, Some(&self.focus_id), 0)
                    } else {
                        FailureResult {
                            action: FailureAction::Stop,
                            failure_mode: Some("F_SYSTEM".to_owned()),
                            plain_language: conductor_err.plain_language().to_owned(),
                            is_recoverable: false,
                            severity: FailureSeverity::Stop,
                            action_id: None,
                            focus_id: Some(self.focus_id.clone()),
                            metadata: None,
                        }
                    };
                    self.write_focus_run_record_logged("failed").await;
                    self.personal_track = None;
                    self.task_track = None;
                    self.shared_state = None;
                    self.privacy_gateway = None;
                    self.emit_status("failed", None);
                    return Ok(RunResult {
                        focus_run_id: self.focus_run_id.clone().unwrap_or_default(),
                        status: "failed".to_owned(),
                        output_id: None,
                        output_content: None,
                        failure: Some(failure),
                        // F_SYSTEM (TaxonomyIntegrity/DatabaseMigration) is a
                        // system/data-integrity failure, not a Phase 4 step
                        // outcome -- out of scope for items.id=297's crisis
                        // floor coverage, same as Phase 1/2 LOAD/AUTHORIZE.
                        crisis_resource_block: None,
                    });
                }

                // Other errors: cleanup state and propagate.
                if self.focus_run_id.is_some() {
                    self.write_focus_run_record_logged("failed").await;
                }
                self.personal_track = None;
                self.task_track = None;
                self.shared_state = None;
                self.privacy_gateway = None;
                self.emit_status("failed", None);
                Err(e)
            }
        }
    }

    /// Precondition: LOAD and AUTHORIZE have already run (`self.focus_run_id`
    /// is Some). Both real callers (submit_focus_run, messages::send_message)
    /// go through `load_and_authorize_run()` synchronously before obtaining a
    /// `FocusRun` to call `execute_full()` on, specifically so the resulting
    /// run_id is available immediately (see that function's own doc comment).
    /// This method used to re-run load()+authorize() unconditionally, which
    /// silently regenerated focus_run_id with a fresh UUID -- every caller's
    /// already-returned run_id then pointed at a focus_runs row nothing ever
    /// wrote output under. In send_message this meant get_output_for_run()
    /// always missed (wrong run_id), the assistant placeholder was never
    /// backfilled, and the chat UI's "Generating..." spinner never resolved
    /// even though the run completed and produced real output under the
    /// second, orphaned run_id (items.id=314, root-caused via live repro
    /// 2026-08-23 -- confirmed by log trace showing two distinct focus_run_id
    /// values for one send_message call, plus the resulting "finished but
    /// produced no output to backfill" warning).
    async fn execute_full_inner(&mut self) -> Result<RunResult, LifecycleError> {
        assert!(
            self.focus_run_id.is_some(),
            "execute_full() requires load()+authorize() to have already run"
        );
        self.initialize().await?;
        self.execute_and_finish().await
    }

    /// Phase 4 EXECUTE onward: run remaining steps from `self.current_step`,
    /// then Phase 5 OUTPUT, the extraction pass, and cleanup.
    ///
    /// Split out of execute_full_inner() (items.id=245) so resume_execution()
    /// (below) can share this tail without re-running initialize() --
    /// initialize() unconditionally resets task_track/shared_state to empty
    /// and current_step is never touched by it, so calling it again on a
    /// rehydrated run would silently discard the state rehydrate_focus_run()
    /// just restored. execute_full_inner() still calls initialize() first,
    /// same as before this split; only the post-initialize tail moved.
    async fn execute_and_finish(&mut self) -> Result<RunResult, LifecycleError> {
        if let Some(early_result) = self.execute().await? {
            let status = early_result.status.clone();
            self.cleanup(&status).await;
            return Ok(early_result);
        }

        // Capture step outputs while task_track is still live.
        // cleanup() later releases task_track, so extraction input
        // must be captured before output() completes.
        let step_outputs: Vec<(String, String)> = match (&self.task_track, &self.focus_def) {
            (Some(tt), Some(fd)) => extraction_step_outputs(tt, fd),
            _ => Vec::new(),
        };

        // Phase 5 OUTPUT -- runs unconditionally before the extract pass.
        // output() saves content, writes run_history, and sets status='awaiting_feedback'.
        // If candidates found below, cleanup("awaiting_extract_confirm") overwrites that.
        // If no candidates, cleanup("complete") overwrites to 'complete'.
        // Either way, output content is correctly saved before parking or completion.
        let output_result = self.output().await?;

        // Build excluded fields: focus field_requirements + existing personal.db field names.
        // Field names only -- values never leave the PersonalTrack boundary.
        // Deduplicated via HashSet to avoid redundant model prompt context.
        let excluded_fields: Vec<String> = {
            use crate::persistence::personal_store::list_personal_fields;
            use std::collections::HashSet;

            let key_hex = self.key_hex.as_deref().ok_or(LifecycleError::NoKey)?;

            let focus_fields: Vec<String> = self
                .focus_def
                .as_ref()
                .map(|fd| {
                    fd.steps
                        .iter()
                        .flat_map(|s| s.field_requirements.keys().cloned())
                        .collect()
                })
                .unwrap_or_default();

            let existing: Vec<String> =
                list_personal_fields(&self.user_id, &self.persona_id, key_hex, None, None)
                    .await
                    .unwrap_or_default()
                    .into_iter()
                    .map(|f| f.field_name)
                    .collect();

            focus_fields
                .into_iter()
                .chain(existing)
                .collect::<HashSet<_>>()
                .into_iter()
                .collect()
        };

        let ollama = crate::providers::ollama_client::OllamaClient::new();
        let candidates = crate::conductor::extract::extract_candidates(
            &step_outputs,
            &excluded_fields,
            &ollama,
            crate::conductor::extract::SOURCE_CONDUCTOR_BATCH,
        )
        .await;

        if !candidates.is_empty() {
            // Invariant: focus_run_id must be set by AUTHORIZE before we reach here.
            let focus_run_id = self.focus_run_id.clone().ok_or_else(|| {
                LifecycleError::ValidationFailed(
                    "focus_run_id missing during extraction pass".to_owned(),
                )
            })?;
            let key_hex = self.key_hex.clone().ok_or(LifecycleError::NoKey)?;

            // Persist candidates to outputs.db.
            // INVARIANT: persist_candidates() returns ids in the same order as
            // the input candidates slice. zip() below relies on this ordering.
            // If persist_candidates() is ever refactored to batch or reorder,
            // update it to return Vec<(i64, ExtractCandidate)> instead.
            let persisted_ids = crate::conductor::extract::persist_candidates(
                &self.user_id,
                &self.persona_id,
                &key_hex,
                &focus_run_id,
                &candidates,
            )
            .await
            .map_err(LifecycleError::Database)?;

            // Build IPC payload and emit push event.
            use crate::conductor::privacy::types::ExtractedCandidate as IpcCandidate;
            let ipc_candidates: Vec<IpcCandidate> = candidates
                .iter()
                .zip(persisted_ids.iter())
                .map(|(c, &id)| IpcCandidate {
                    candidate_id: id,
                    field_name: c.field_name.clone(),
                    extracted_value: c.extracted_value.clone(),
                    sensitivity: c.sensitivity.clone(),
                    reason: c.reason.clone(),
                    confidence: c.confidence,
                    warn_flag: c.warn_flag,
                    source: Some(c.source.clone()),
                })
                .collect();

            if let Some(handle) = &self.app_handle {
                use tauri::Emitter;
                let payload = serde_json::json!({
                    "run_id": focus_run_id,
                    "focus_id": self.focus_id,
                    "candidates": serde_json::to_value(&ipc_candidates)
                        .unwrap_or(serde_json::json!([])),
                });
                if let Err(e) = handle.emit("extract_confirm_request", &payload) {
                    log::warn!("lifecycle: emit extract_confirm_request failed: {e}");
                }
            }

            // Park the run -- frontend must call submit_extract_confirm to resume.
            // cleanup("awaiting_extract_confirm") overwrites 'awaiting_feedback' to
            // 'awaiting_extract_confirm', releases tracks, and preserves snapshots
            // (not in the purge set). Output content and run_history already saved.
            // submit_extract_confirm will set status='complete' when decisions are written.
            self.cleanup("awaiting_extract_confirm").await;

            return Ok(RunResult {
                focus_run_id,
                status: "awaiting_extract_confirm".to_owned(),
                output_id: output_result.output_id,
                output_content: output_result.output_content,
                failure: None,
                // Forward, don't recompute -- output() (above) already ran
                // and already decided this; output_content already has the
                // block appended, this just carries the structured signal
                // through too.
                crisis_resource_block: output_result.crisis_resource_block,
            });
        }

        // No candidates -- output already saved. cleanup("complete") overwrites
        // 'awaiting_feedback' to 'complete' and purges snapshots.
        self.cleanup("complete").await;
        Ok(output_result)
    }

    /// Resume a rehydrated FocusRun: promote it to 'running' and continue
    /// execution from `self.current_step` onward (items.id=245).
    ///
    /// rehydrate_focus_run() (below) deliberately never writes to focus_runs
    /// (see its own doc comment: "its status is the caller's to change, not
    /// this function's") -- this is that write. Mirrors initialize()'s own
    /// write_focus_run_record("running") + emit_status("running", None)
    /// tail, since a rehydrated run skips initialize() entirely (see
    /// execute_and_finish()'s doc comment for why: initialize() would wipe
    /// the just-restored task_track/shared_state).
    ///
    /// Callers must have already produced `self` via rehydrate_focus_run()
    /// -- this method assumes personal_track/task_track/shared_state/
    /// focus_def/focus_run_id/failure_handler/privacy_gateway are all
    /// populated (the same preconditions execute() itself asserts).
    pub async fn resume_execution(&mut self) -> Result<RunResult, LifecycleError> {
        self.write_focus_run_record("running").await?;
        self.emit_status("running", None);
        self.execute_and_finish().await
    }

    /// Named re-entry with delta scope (items.id=574 follow-up, cb-07):
    /// jump back to `target_action_id` with new information, discard the now-
    /// stale downstream steps, and leave the run positioned to resume
    /// forward from there on the next call to execute()/resume_execution().
    ///
    /// Deliberately does NOT call execute() itself -- exactly like
    /// rehydrate_focus_run(), this only rewinds and persists state. A
    /// separate, already-built "fresh execution call" (resume_run's
    /// rehydrate_focus_run() + resume_execution() path) is what actually
    /// continues forward -- the existing execute() loop already starts
    /// from self.current_step unconditionally (crash-resume requires
    /// exactly that), so nothing about it needs to change for reentry.
    ///
    /// Callers must have already produced `self` via rehydrate_focus_run(),
    /// same precondition resume_execution() has.
    ///
    /// focus_step_order is the caller's resolved step list (this run's own
    /// focus_def.steps, in order) -- see reentry::plan_reentry()'s own doc
    /// comment for why this can't be derived from task_track alone.
    pub async fn reenter_step(
        &mut self,
        focus_step_order: &[String],
        target_action_id: &str,
    ) -> Result<ReentryPlan, LifecycleError> {
        let plan = reentry::plan_reentry(
            self.task_track.as_ref().unwrap(),
            focus_step_order,
            target_action_id,
        )?;

        self.task_track = Some(reentry::rebuild_preserved_track(
            self.task_track.as_ref().unwrap(),
            &plan,
        ));

        let focus_run_id = self.focus_run_id.as_deref().unwrap();
        let key_hex = self.key_hex.as_deref().ok_or(LifecycleError::NoKey)?;
        let mut conn = open_outputs_db(&self.user_id, &self.persona_id, key_hex).await?;
        let timestamp = now();

        for action_id in &plan.discarded_action_ids {
            // No-op (0 rows) is expected and fine when this step never got a
            // focus_run_steps row in the first place -- nothing writes rows
            // for ordinary forward execution yet (items.id=574 follow-up,
            // not this dispatch); this UPDATE only ever affects rows a prior
            // reenter_step() call itself inserted.
            sqlx::query(
                "UPDATE focus_run_steps SET status = 'discarded', updated_at = ?
                 WHERE focus_run_id = ? AND action_id = ? AND status != 'discarded'",
            )
            .bind(&timestamp)
            .bind(focus_run_id)
            .bind(action_id)
            .execute(&mut conn)
            .await?;
        }

        // HARD INVARIANT (Jason, items.id=574): focus_run_steps has no
        // superseded_by pointer column -- the schema relies entirely on
        // `attempt` being a monotonic integer per (focus_run_id, action_id),
        // so the successor to any row is always MAX(attempt)+1 for that same
        // key. Nothing in the schema enforces this -- it MUST be computed
        // here, never a caller-supplied or independently-derived value, or
        // the "no pointer column needed" simplification silently breaks.
        let next_attempt: i64 = sqlx::query_scalar(
            "SELECT COALESCE(MAX(attempt), 0) + 1 FROM focus_run_steps
             WHERE focus_run_id = ? AND action_id = ?",
        )
        .bind(focus_run_id)
        .bind(target_action_id)
        .fetch_one(&mut conn)
        .await?;

        sqlx::query(
            "INSERT INTO focus_run_steps
             (id, focus_run_id, action_id, attempt, sequence_index, status, created_at, updated_at)
             VALUES (?, ?, ?, ?, ?, 'pending', ?, ?)",
        )
        .bind(Uuid::new_v4().to_string())
        .bind(focus_run_id)
        .bind(target_action_id)
        .bind(next_attempt)
        .bind(plan.resume_from_index as i64)
        .bind(&timestamp)
        .bind(&timestamp)
        .execute(&mut conn)
        .await?;

        drop(conn);

        self.current_step = plan.resume_from_index;

        // Required, not cosmetic: rehydrate_focus_run()'s own resume-index
        // derivation only resumes AT the checkpointed step when status ==
        // "awaiting_user" -- every other status resumes AFTER it (i + 1).
        // We are about to checkpoint at target_action_id and need a future
        // rehydrate (e.g. after a crash before the next execute() call) to
        // land exactly there, not one step later.
        self.write_focus_run_record("awaiting_user").await?;
        self.write_checkpoint(target_action_id).await?;

        Ok(plan)
    }
}

// ---------------------------------------------------------------------------
// demote_interrupted_runs
// ---------------------------------------------------------------------------

/// Update stale in-progress focus_runs to 'paused' status.
/// Called at startup to recover from sessions interrupted by crash or restart.
/// Returns `Some(rows updated)`, or `None` when the demote was deferred
/// because the user has a chat reply or Focus run alive (items.id=794).
///
/// items.id=794 (decisions.id=902): since items.id=781 a reply or Focus run
/// can outlive its session and finish saving under its own writer lease, so a
/// `running` row older than the threshold may belong to a run that is alive in
/// this process. A fresh process after a crash has no leases, so a run left
/// behind by a crash is still demoted. The lease is keyed by user and kind, not
/// by run or persona, so any live visible lease for the user defers the whole
/// persona; the next login with no visible work demotes it (idempotent). There
/// is no recheck in between. The check sits right before the UPDATE, after the
/// database is open, to keep the window short in which a run resumed in the
/// meantime could still be demoted (residual, accepted: the run overwrites the
/// status when it ends).
/// Python oracle: demote_interrupted_runs() standalone function.
pub async fn demote_interrupted_runs(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
) -> Result<Option<u64>, LifecycleError> {
    let threshold_minutes: i64 = std::env::var("QR_INTERRUPT_THRESHOLD_MINUTES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(5);

    let mut conn = open_outputs_db(user_id, persona_id, key_hex).await?;

    // items.id=587 fix (2026-10-03): started_at is written by
    // providers::utils::now() as an RFC3339 string with a 'T' date/time
    // separator and a '+00:00' offset suffix (chrono's to_rfc3339()).
    // datetime('now', ...) instead produces SQLite's own space-separated,
    // offset-less format. Comparing those two TEXT values with `<` is a
    // plain byte-wise comparison -- 'T' (0x54) sorts after a space
    // (0x20), so started_at always read as lexicographically "later" than
    // the datetime() cutoff, regardless of the real elapsed time. This
    // made the WHERE clause match zero rows, always, at any threshold --
    // confirmed live (2026-10-03): a run left at status='running' by a
    // killed process was still read back as 'running' after 6+ real
    // minutes and two logins, one of them with
    // QR_INTERRUPT_THRESHOLD_MINUTES=0. unixepoch() normalizes both sides
    // to integer seconds regardless of which ISO8601-family format they
    // started in, sidestepping the format mismatch entirely.
    //
    // items.id=794: only chat replies and Focus runs count (`visible()`); the
    // login's own Command lease and the recovery's own Sweep lease do not.
    let live_visible = crate::auth::writer_gate::counts(user_id).visible();
    if live_visible > 0 {
        log::info!(
            "demote_interrupted_runs: deferring for user={user_id} persona={persona_id}: \
             {live_visible} reply/Focus run lease(s) live"
        );
        return Ok(None);
    }
    let result = sqlx::query(
        "UPDATE focus_runs SET status = 'paused'
         WHERE status IN ('running', 'initializing')
         AND unixepoch(started_at) < unixepoch('now', ? || ' minutes')",
    )
    .bind(format!("-{threshold_minutes}"))
    .execute(&mut conn)
    .await?;

    Ok(Some(result.rows_affected()))
}

// ---------------------------------------------------------------------------
// rehydrate_focus_run
// ---------------------------------------------------------------------------

/// Reconstruct a live, execute()-ready FocusRun from persisted state: the
/// focus_runs row (focus_id, status, is_fast_lane, is_quick_ask, topic_id,
/// user_input) plus the latest focus_run_snapshots row for this run
/// (task_track_json/shared_state_json), plus a FRESH PersonalTrack fetch.
///
/// PersonalTrack is never deserialized from a snapshot — re-fetched fresh
/// here exactly as initialize() does for a brand-new run. That invariant
/// (see this file's module doc comment and write_checkpoint()'s own doc
/// comment) is unchanged by this function.
///
/// Shared foundation for items.id=245 (resume_run's not_implemented arms)
/// and this item's own follow-up dispatch (reenter_step(), not built yet).
/// This function only rebuilds the FocusRun object in memory — it never
/// writes to focus_runs (the row already exists; its status is the
/// caller's to change, not this function's) and never calls execute().
///
/// current_step derivation: the checkpointed action_id is looked up in
/// focus_def.steps to find its index. The two call sites that write a
/// checkpoint (write_checkpoint(), this file) leave that step in different
/// states:
///   - the Tier 3 handoff-pause checkpoint (execute()'s requires_user_handoff
///     branch) and the schedule_trigger gate's checkpoint (items.id=496,
///     same branch shape) are both written for the step BEFORE it runs,
///     then the run is parked at status='awaiting_user' or
///     'awaiting_schedule' respectively — resume must re-enter at that same
///     index in either case.
///   - the periodic mid-run checkpoint (execute()'s checkpoint_every branch)
///     is written AFTER a step completes — resume must continue at the next
///     index. A crash between checkpoints leaves status='running'/
///     'initializing', which demote_interrupted_runs() (above) sweeps to
///     'paused' at next startup before anything ever calls this function.
///
/// So: status=='awaiting_user'/'awaiting_schedule' resumes AT the
/// checkpointed step; any other status resumes AFTER it. No snapshot row at
/// all means the run was interrupted before its first checkpoint — current_step stays 0, exactly
/// like a fresh run. A action_id that no longer matches any step in the
/// current focus_def (the .focus file changed underneath the run) also
/// falls back to 0 rather than erroring — the same "re-read fresh" posture
/// this item's design already takes for composition changes.
///
/// key_hex/pool/scheduler/app_handle/confirmed_cross_persona_fact_ids are
/// caller-supplied, not persisted — same reasoning FocusRun's own field
/// doc comments already give for why each of those lives only in memory.
#[allow(clippy::too_many_arguments)] // Explicit architecture boundary; see D6-342/D6-346 (matches FocusRun::new()).
pub async fn rehydrate_focus_run<L: DisclosureLoggerForRun>(
    user_id: String,
    persona_id: String,
    focus_run_id: String,
    pool: sqlx::SqlitePool,
    scheduler: Arc<ConductorScheduler>,
    key_hex: Option<String>,
    confirmed_cross_persona_fact_ids: std::collections::HashSet<String>,
    app_handle: Option<tauri::AppHandle<tauri::Wry>>,
) -> Result<FocusRun<L>, LifecycleError> {
    let key = match key_hex.as_deref() {
        Some(k) if !k.is_empty() => k,
        _ => return Err(LifecycleError::NoKey),
    };

    let mut conn = open_outputs_db(&user_id, &persona_id, key).await?;

    let run_row = sqlx::query(
        "SELECT focus_id, status, is_fast_lane, is_quick_ask, topic_id, user_input
         FROM focus_runs WHERE id = ?",
    )
    .bind(&focus_run_id)
    .fetch_optional(&mut conn)
    .await?
    .ok_or_else(|| LifecycleError::RunNotFound(focus_run_id.clone()))?;

    let focus_id: String = run_row.try_get("focus_id")?;
    let status: String = run_row.try_get("status")?;
    let is_fast_lane: bool = run_row.try_get::<i64, _>("is_fast_lane")? != 0;
    let is_quick_ask: bool = run_row.try_get::<i64, _>("is_quick_ask")? != 0;
    let topic_id: Option<String> = run_row.try_get("topic_id")?;
    let user_input: String = run_row
        .try_get::<Option<String>, _>("user_input")?
        .unwrap_or_default();

    let snapshot_row = sqlx::query(
        "SELECT action_id, task_track_json, shared_state_json
         FROM focus_run_snapshots
         WHERE focus_run_id = ?
         ORDER BY created_at DESC
         LIMIT 1",
    )
    .bind(&focus_run_id)
    .fetch_optional(&mut conn)
    .await?;

    drop(conn);

    let mut run = FocusRun::<L>::new(
        user_id,
        persona_id,
        focus_id,
        pool,
        scheduler,
        user_input,
        is_fast_lane,
        Some(key.to_owned()),
        topic_id,
        is_quick_ask,
        confirmed_cross_persona_fact_ids,
        app_handle,
    );
    run.focus_run_id = Some(focus_run_id);

    // items.id=716: a resumed API Chat run goes back to the destination its
    // conversation was bound to, never to whatever the user's preference
    // happens to resolve to now. With no readable binding it does not run.
    if run.focus_id == crate::api_chat::API_CHAT_FOCUS_ID {
        let run_id = run.focus_run_id.clone().unwrap_or_default();
        let bound =
            crate::api_chat::destination_for_run(&run.user_id, &run.persona_id, key, &run_id)
                .await
                .map_err(|code| LifecycleError::ValidationFailed(code.as_str().to_owned()))?;
        run.chat_binding = Some(bound);
    }

    run.load().await?;
    run.resolve_tier_config().await?;
    run.failure_handler = Some(run.build_failure_handler().await);

    let mut personal_track = run.build_personal_track().await?;
    personal_track.seal();
    run.personal_track = Some(personal_track);
    run.privacy_gateway = Some(PrivacyGateway::new(L::for_run(
        &run.user_id,
        &run.persona_id,
        key,
    )));
    run._persona_context_rendered = run.assemble_persona_context().await;

    let (task_track, shared_state, current_step) = match snapshot_row {
        Some(row) => {
            let action_id: String = row.try_get("action_id")?;
            let task_json: String = row.try_get("task_track_json")?;
            let shared_json: String = row.try_get("shared_state_json")?;

            let task_track = deserialize_task_track(&task_json)?;
            let shared_state = deserialize_shared_state_track(&shared_json)?;

            let resume_at =
                resume_index(&run.focus_def.as_ref().unwrap().steps, &action_id, &status);
            (task_track, shared_state, resume_at)
        }
        None => (TaskTrack::new(), SharedStateTrack::new(), 0),
    };

    run.task_track = Some(task_track);
    run.shared_state = Some(shared_state);
    run.current_step = current_step;

    Ok(run)
}

/// Gate3 look-ahead: the execution tier of the step after `step_index`, under
/// `ceiling` (the Focus-level cap).
///
/// items.id=878: a guarded row may be skipped, and which row runs next can
/// depend on the current step's own output, so this takes the HIGHEST tier among
/// every row that could run next: the following rows up to and including the
/// first unguarded one. For a Focus with no guards that is exactly the single
/// following row, as before. Never lower than the real next step, so Gate3 can
/// fire when it need not but cannot be missed.
fn next_execution_tier(
    steps: &[ActionDefinition],
    step_index: usize,
    ceiling: ExternalAccess,
) -> Option<u8> {
    let upcoming = steps.get(step_index + 1..).unwrap_or_default();
    let mut candidates = Vec::new();
    for next in upcoming {
        // items.id=887: a store writer makes no call and sends nothing, so it
        // is neither a candidate nor a row that ends the scan.
        if next.store_op.is_some() {
            continue;
        }
        candidates.push(next);
        if !next.is_guarded() {
            break;
        }
    }
    candidates
        .iter()
        .map(|next| execution_tier_ordinal(ExternalAccess::min(ceiling, next.routing_tier)))
        .max()
}

/// Where a rehydrated run resumes: the index of the step after the
/// checkpointed one, or AT it for a run parked on that very step.
///
/// items.id=496: awaiting_schedule's checkpoint is written for the step BEFORE
/// it runs, same as awaiting_user's handoff-pause checkpoint (see execute()'s
/// schedule_trigger gate) -- resume must re-enter at that same index, not the
/// next one, or the step whose trigger just fired would be skipped entirely.
///
/// items.id=878: unchanged by guards. A checkpoint is only ever written for a
/// step that ran (a skipped step writes none), and the rows between it and the
/// next executed one are simply reached again, where their guards are evaluated
/// over the restored track and give the same answer as before the interruption.
/// Extracted from rehydrate_focus_run() unchanged so a test can drive the rule.
fn resume_index(steps: &[ActionDefinition], action_id: &str, status: &str) -> usize {
    match steps.iter().position(|s| s.action_id == action_id) {
        Some(i) if status == "awaiting_user" || status == "awaiting_schedule" => i,
        Some(i) => i + 1,
        None => 0,
    }
}

/// Rebuild a TaskTrack from write_checkpoint()'s stored JSON shape
/// (`{"steps": [...], "sensitivity_ceiling": N}`) by replaying add_step()
/// for each stored step in order — add_step() is TaskTrack's only writer,
/// so this reproduces output_vars and sensitivity_ceiling identically to
/// the original run rather than trying to poke private fields directly.
fn deserialize_task_track(json: &str) -> Result<TaskTrack, LifecycleError> {
    #[derive(Deserialize)]
    struct StoredTaskData {
        steps: Vec<TaskStep>,
    }
    let stored: StoredTaskData = serde_json::from_str(json)?;
    let mut track = TaskTrack::new();
    for step in stored.steps {
        track.add_step(step);
    }
    Ok(track)
}

/// Rebuild a SharedStateTrack from write_checkpoint()'s stored JSON shape
/// (`{"step_disclosure_buffers": {...}, "promotions": [...]}`), replaying
/// write_disclosure_buffer()/promote_content() — same reasoning as
/// deserialize_task_track() above. Promotions are replayed in their
/// stored (append) order so get_promoted()'s "most recent wins" search
/// still resolves the same way it did in the original run.
fn deserialize_shared_state_track(json: &str) -> Result<SharedStateTrack, LifecycleError> {
    #[derive(Deserialize)]
    struct StoredSharedData {
        step_disclosure_buffers: HashMap<String, HashMap<String, String>>,
        promotions: Vec<PromotedContentEntry>,
    }
    let stored: StoredSharedData = serde_json::from_str(json)?;
    let mut track = SharedStateTrack::new();
    for (action_id, fields) in stored.step_disclosure_buffers {
        track.write_disclosure_buffer(action_id, fields);
    }
    for p in stored.promotions {
        track.promote_content(p.action_id, p.content_key, p.content);
    }
    Ok(track)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// A lazy, never-connected shared.db pool (items.id=483) for FocusRun
    /// test fixtures that never exercise a DB-touching method
    /// (get_focus_tier_ceiling/authorize, or execute()'s tier>=1 model
    /// selection) -- every fixture below stops well short of those paths
    /// (pure in-memory methods, or execute() on a pre-sealed cloud_frontier run,
    /// which is a terminal boundary that returns before StepExecutor is
    /// ever reached). connect_lazy_with never opens a real connection until
    /// something is actually acquired against it, so this is safe to
    /// construct freely without a tempdir/QR_DATA_ROOT.
    fn dummy_pool() -> sqlx::SqlitePool {
        // idle_timeout/max_lifetime/min_connections are disabled here: sqlx's
        // default PoolOptions spawn a background reaper task unconditionally
        // on pool creation (even for a lazy pool), which requires a Tokio
        // runtime to be current. Several of this module's fixtures
        // (output_sensitivity_* below) are plain #[test] functions with no
        // Tokio runtime -- without disabling these options, constructing the
        // pool itself panics with "this functionality requires a Tokio
        // context" before any query is ever run.
        sqlx::sqlite::SqlitePoolOptions::new()
            .idle_timeout(None)
            .max_lifetime(None)
            .min_connections(0)
            .connect_lazy_with(sqlx::sqlite::SqliteConnectOptions::new().filename(":memory:"))
    }

    // -------------------------------------------------------------------------
    // parse_focus_definition
    // -------------------------------------------------------------------------

    fn minimal_raw() -> RawFocusFile {
        RawFocusFile {
            id: Some("quick-ask".to_owned()),
            display_name: Some("Quick Ask".to_owned()),
            description: Some("Fast single-step query".to_owned()),
            version: Some(serde_json::Value::String("1.0".to_owned())),
            max_routing_tier: Some(1),
            output_types: Some(vec!["quick_ask".to_owned()]),
            output_type: None,
            guides: Some(vec!["quick-ask-guide".to_owned()]),
            suggest_in_focuses: None,
            multi_source_validation: None,
            steps: None,
            brief: None,
            display_config: None,
        }
    }

    #[test]
    fn parse_minimal_focus() {
        let def = parse_focus_definition(minimal_raw()).unwrap();
        assert_eq!(def.focus_id, "quick-ask");
        assert_eq!(def.display_name, "Quick Ask");
        assert_eq!(def.output_type, "quick_ask");
        assert_eq!(def.version, "1.0");
        assert_eq!(def.max_routing_tier, ExternalAccess::LocalOnly);
        assert!(def.steps.is_empty());
        assert!(!def.multi_source_validation);
        assert!(def.suggest_in_focuses.is_empty());
        assert_eq!(
            def.generic_title_template, "Hidden item",
            "no display_config declared -- platform default applies"
        );
    }

    #[test]
    fn parse_generic_title_template_from_display_config() {
        let mut r = minimal_raw();
        r.display_config = Some(RawDisplayConfig {
            generic_title_template: Some("A {entity_type} record".to_owned()),
            high_priority_trigger: None,
        });
        assert_eq!(
            parse_focus_definition(r).unwrap().generic_title_template,
            "A {entity_type} record"
        );
    }

    #[test]
    fn parse_generic_title_template_defaults_when_display_config_present_but_empty() {
        // decisions.id=513: template "must handle full range of object
        // states" -- an explicitly present but empty display_config section
        // must still fall back to the platform default, not an empty string.
        let mut r = minimal_raw();
        r.display_config = Some(RawDisplayConfig {
            generic_title_template: None,
            high_priority_trigger: None,
        });
        assert_eq!(
            parse_focus_definition(r).unwrap().generic_title_template,
            "Hidden item"
        );
    }

    // -------------------------------------------------------------------------
    // parse_offset / high_priority_trigger (decisions.id=712, items.id=236)
    // -------------------------------------------------------------------------

    #[test]
    fn parse_offset_valid_values() {
        assert_eq!(parse_offset("-4h").unwrap(), Duration::hours(-4));
        assert_eq!(parse_offset("+0").unwrap(), Duration::zero());
        assert_eq!(parse_offset("0").unwrap(), Duration::zero());
        assert_eq!(parse_offset("-0").unwrap(), Duration::zero());
        assert_eq!(parse_offset("-1d").unwrap(), Duration::days(-1));
        assert_eq!(parse_offset("+2h").unwrap(), Duration::hours(2));
        assert_eq!(parse_offset("3d").unwrap(), Duration::days(3));
    }

    #[test]
    fn parse_offset_rejects_malformed_values() {
        for bad in ["-4", "h4", "+1w", "", "+", "-", "4hh", "d"] {
            assert!(
                parse_offset(bad).is_err(),
                "expected {bad:?} to be rejected"
            );
        }
    }

    #[test]
    fn parse_high_priority_trigger_from_display_config() {
        // Synthetic fixture modeled after Travel's departure-time anchor
        // (decisions.id=712) — no travel.focus file exists in this repo
        // (items.id=236 scope correction).
        let mut r = minimal_raw();
        r.display_config = Some(RawDisplayConfig {
            generic_title_template: None,
            high_priority_trigger: Some(RawHighPriorityTrigger {
                anchor_field: "departure_time".to_owned(),
                offset: "-4h".to_owned(),
            }),
        });
        let trigger = parse_focus_definition(r)
            .unwrap()
            .high_priority_trigger
            .unwrap();
        assert_eq!(trigger.anchor_field, "departure_time");
        assert_eq!(trigger.offset, Duration::hours(-4));
    }

    #[test]
    fn parse_high_priority_trigger_zero_offset_accepts_no_unit() {
        // Synthetic fixture modeled after Habit's due-date anchor
        // (decisions.id=712) — no habit.focus file exists in this repo
        // (items.id=236 scope correction).
        let mut r = minimal_raw();
        r.display_config = Some(RawDisplayConfig {
            generic_title_template: None,
            high_priority_trigger: Some(RawHighPriorityTrigger {
                anchor_field: "due_date".to_owned(),
                offset: "+0".to_owned(),
            }),
        });
        let trigger = parse_focus_definition(r)
            .unwrap()
            .high_priority_trigger
            .unwrap();
        assert_eq!(trigger.anchor_field, "due_date");
        assert_eq!(trigger.offset, Duration::zero());
    }

    #[test]
    fn parse_high_priority_trigger_rejects_malformed_offset() {
        let mut r = minimal_raw();
        r.display_config = Some(RawDisplayConfig {
            generic_title_template: None,
            high_priority_trigger: Some(RawHighPriorityTrigger {
                anchor_field: "due_date".to_owned(),
                offset: "not-an-offset".to_owned(),
            }),
        });
        let err = parse_focus_definition(r).unwrap_err();
        assert!(matches!(err, LifecycleError::ValidationFailed(_)));
    }

    #[test]
    fn parse_focus_definition_with_no_display_config_has_no_trigger() {
        // Regression guard: none of today's four real .focus files declare
        // display_config.high_priority_trigger — they must keep parsing to
        // high_priority_trigger: None.
        assert!(parse_focus_definition(minimal_raw())
            .unwrap()
            .high_priority_trigger
            .is_none());
    }

    #[test]
    fn high_priority_trigger_is_active_boundary_is_inclusive() {
        let trigger = HighPriorityTrigger {
            anchor_field: "departure_time".to_owned(),
            offset: Duration::hours(-4),
        };
        let anchor = "2026-08-10T12:00:00Z".parse::<DateTime<Utc>>().unwrap();

        // Exactly at the threshold (anchor + offset) -- inclusive.
        assert!(trigger.is_active(anchor, anchor + Duration::hours(-4)));
        // One second before the threshold -- not yet active.
        assert!(!trigger.is_active(anchor, anchor + Duration::hours(-4) - Duration::seconds(1)));
        // Exactly at the anchor -- active (window is open-ended forward).
        assert!(trigger.is_active(anchor, anchor));
        // Long past the anchor -- still active, no upper bound
        // (items.id=236 plan judgment call 4: exit is via lifecycle
        // transition, not the clock).
        assert!(trigger.is_active(anchor, anchor + Duration::days(30)));
    }

    #[test]
    fn parse_output_type_fallback_chain() {
        let mut r = minimal_raw();
        r.output_types = Some(vec!["research_report".to_owned()]);
        r.output_type = Some("ignored".to_owned());
        assert_eq!(
            parse_focus_definition(r).unwrap().output_type,
            "research_report"
        );

        let mut r2 = minimal_raw();
        r2.output_types = None;
        r2.output_type = Some("essay".to_owned());
        assert_eq!(parse_focus_definition(r2).unwrap().output_type, "essay");

        let mut r3 = minimal_raw();
        r3.output_types = None;
        r3.output_type = None;
        assert_eq!(parse_focus_definition(r3).unwrap().output_type, "general");
    }

    #[test]
    fn parse_step_inherits_focus_level_guide() {
        let mut steps_map = IndexMap::new();
        steps_map.insert(
            "step_a".to_owned(),
            RawStep {
                display_name: Some("Step A".to_owned()),
                guide_id: None,
                task_type: Some("general".to_owned()),
                routing_tier: Some(1),
                requires_user_handoff: None,
                step_type: None,
                output_var: Some("result".to_owned()),
                prompt_template: Some("Hello {user_input}".to_owned()),
                field_requirements: None,
                options_override: None,
                declared_inputs: None,
            },
        );
        let mut raw = minimal_raw();
        raw.guides = Some(vec!["custom-guide".to_owned()]);
        raw.steps = Some(steps_map);

        let def = parse_focus_definition(raw).unwrap();
        assert_eq!(def.steps.len(), 1);
        assert_eq!(def.steps[0].guide_id, "custom-guide");
        assert_eq!(def.steps[0].action_id, "step_a");
        assert_eq!(def.steps[0].output_var.as_deref(), Some("result"));
    }

    #[test]
    fn parse_derives_revisitable_from_step_type() {
        // Exercises the real production derivation (parse_focus_definition(),
        // not just tokens.rs's StepType::default_revisitable() unit tests)
        // for all 3 step_types in one fixture.
        let mut steps_map = IndexMap::new();
        for (id, raw_type) in [
            ("s_generate", "generate"),
            ("s_voice", "voice_transform"),
            ("s_post", "post_process"),
        ] {
            steps_map.insert(
                id.to_owned(),
                RawStep {
                    display_name: None,
                    guide_id: None,
                    task_type: None,
                    routing_tier: None,
                    requires_user_handoff: None,
                    step_type: Some(raw_type.to_owned()),
                    output_var: None,
                    prompt_template: None,
                    field_requirements: None,
                    options_override: None,
                    declared_inputs: None,
                },
            );
        }
        let mut raw = minimal_raw();
        raw.steps = Some(steps_map);
        let def = parse_focus_definition(raw).unwrap();

        let revisitable = |id: &str| {
            def.steps
                .iter()
                .find(|s| s.action_id == id)
                .unwrap()
                .revisitable
        };
        assert!(revisitable("s_generate"));
        assert!(revisitable("s_voice"));
        assert!(!revisitable("s_post"));
    }

    #[test]
    fn parse_no_guides_falls_back_to_quick_ask_guide() {
        let mut steps_map = IndexMap::new();
        steps_map.insert(
            "step_x".to_owned(),
            RawStep {
                display_name: None,
                guide_id: None,
                task_type: None,
                routing_tier: None,
                requires_user_handoff: None,
                step_type: None,
                output_var: None,
                prompt_template: None,
                field_requirements: None,
                options_override: None,
                declared_inputs: None,
            },
        );
        let mut raw = minimal_raw();
        raw.guides = None;
        raw.steps = Some(steps_map);

        let def = parse_focus_definition(raw).unwrap();
        assert_eq!(def.steps[0].guide_id, "quick-ask-guide");
    }

    // -- requires_user_handoff / external_access_override decoupling
    //    (items.id=528 Phase 2) --------------------------------------------
    //
    // Before this fix, external_access_override was derived from
    // requires_user_handoff (itself always == routing_tier==3), so the two
    // could never actually diverge. These tests exercise the real
    // parse_focus_definition() derivation -- not just tokens.rs's test
    // mirror -- for the case that coupling would have gotten wrong: a step
    // whose routing_tier is NOT Unrestricted but which still declares
    // requires_user_handoff: true via its own .focus YAML field.

    fn single_step_raw(routing_tier: Option<u8>, requires_user_handoff: Option<bool>) -> RawStep {
        RawStep {
            display_name: Some("Step A".to_owned()),
            guide_id: None,
            task_type: Some("general".to_owned()),
            routing_tier,
            requires_user_handoff,
            step_type: None,
            output_var: Some("result".to_owned()),
            prompt_template: Some("Hello {user_input}".to_owned()),
            field_requirements: None,
            options_override: None,
            declared_inputs: None,
        }
    }

    fn parse_single_step_focus(
        routing_tier: Option<u8>,
        requires_user_handoff: Option<bool>,
    ) -> ActionDefinition {
        let mut steps_map = IndexMap::new();
        steps_map.insert(
            "step_a".to_owned(),
            single_step_raw(routing_tier, requires_user_handoff),
        );
        let mut raw = minimal_raw();
        raw.max_routing_tier = Some(3); // permissive ceiling -- these tests exercise the step's own derivation, not the authorize()-time ceiling check
        raw.steps = Some(steps_map);
        parse_focus_definition(raw).unwrap().steps.remove(0)
    }

    #[test]
    fn requires_user_handoff_absent_falls_back_to_routing_tier_3() {
        // Back-compat: a .focus file authored before this field existed
        // (every shipped Focus today) must behave identically.
        let step = parse_single_step_focus(Some(3), None);
        assert!(step.requires_user_handoff);
        assert_eq!(step.external_access_override, None);
    }

    #[test]
    fn requires_user_handoff_true_at_anonymous_required_keeps_override() {
        let step = parse_single_step_focus(Some(2), Some(true));
        assert!(step.requires_user_handoff);
        assert_eq!(
            step.external_access_override,
            Some(ExternalAccess::AnonymousRequired)
        );
    }

    #[test]
    fn requires_user_handoff_true_at_local_only_keeps_override() {
        let step = parse_single_step_focus(Some(1), Some(true));
        assert!(step.requires_user_handoff);
        assert_eq!(
            step.external_access_override,
            Some(ExternalAccess::LocalOnly)
        );
    }

    // -- YAML parser parity (items.id=733) ----------------------------------
    //
    // The snapshots in tests/golden/focus_parse/ were generated with serde_yaml
    // BEFORE the parser was replaced (RUSTSEC-2024-0449). Every shipped .focus
    // file and every fixture under tests/golden/focus_parse/fixtures/ must
    // still produce the same FocusDefinition, byte for byte, or the same Err.
    // Regenerate (only on a deliberate behaviour change):
    //   cargo test regenerate_focus_parse_snapshots -- --ignored

    fn focus_parse_golden_dir() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden/focus_parse")
    }

    /// (golden file name, .focus text) for every shipped focus file and every
    /// fixture, in a stable order.
    fn focus_parse_inputs() -> Vec<(String, String)> {
        let shipped =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../app/core_artifacts/focuses");
        let fixtures = focus_parse_golden_dir().join("fixtures");
        let mut out = Vec::new();
        for (prefix, dir) in [("shipped", shipped), ("fixture", fixtures)] {
            let mut names: Vec<_> = std::fs::read_dir(&dir)
                .unwrap_or_else(|e| panic!("read {dir:?}: {e}"))
                .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
                .filter(|n| n.ends_with(".focus"))
                .collect();
            names.sort();
            for n in names {
                let text = std::fs::read_to_string(dir.join(&n)).unwrap();
                out.push((format!("{prefix}__{n}.txt"), text));
            }
        }
        out
    }

    /// A deterministic rendering of everything parsing produced. HashMaps are
    /// sorted; every other field goes through Debug.
    fn render_focus_snapshot(r: &Result<FocusDefinition, LifecycleError>) -> String {
        use std::collections::BTreeMap;
        let def = match r {
            // The error TEXT belongs to the parser and is not compared.
            Err(_) => return "ERR\n".to_owned(),
            Ok(def) => def,
        };
        let mut out = String::from("OK\n");
        let mut shell = def.clone();
        shell.steps.clear();
        out += &format!("{shell:#?}\n");
        for step in &def.steps {
            let mut s = step.clone();
            let fr: BTreeMap<_, _> = s.field_requirements.drain().collect();
            let oo: BTreeMap<_, _> = s.options_override.drain().collect();
            out += &format!(
                "--- step\n{s:#?}\nfield_requirements: {fr:?}\noptions_override: {}\n",
                serde_json::to_string(&oo).unwrap()
            );
        }
        out
    }

    // -- hostile input (items.id=733) ----------------------------------------
    //
    // Every case must come back as Err, never a panic, a hang or a stack
    // overflow. Each runs on a thread with a 1 MiB stack, half of a Tokio
    // worker's default 2 MiB, so a parser that recursed per nesting level of a
    // 100_000-deep input would overflow it. (Measured: a debug build needs more
    // than 256 KiB even for a legal 16-deep file, and about 512 KiB is enough
    // for every case here, so 1 MiB leaves headroom without hiding recursion.)

    fn parse_on_small_stack(text: String) -> Result<FocusDefinition, LifecycleError> {
        std::thread::Builder::new()
            .stack_size(1024 * 1024)
            .spawn(move || parse_focus_text(&text))
            .unwrap()
            .join()
            .expect("the parser must not panic or overflow its stack")
    }

    fn assert_rejected(label: &str, text: String) {
        let started = std::time::Instant::now();
        let r = parse_on_small_stack(text);
        assert!(r.is_err(), "{label}: expected Err");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "{label}: rejection must be fast"
        );
        // The message is plain text: one line of prose, no source echo.
        let msg = r.err().unwrap().to_string();
        assert!(
            !msg.contains('\u{1b}'),
            "{label}: no terminal escapes in {msg:?}"
        );
    }

    #[test]
    fn deeply_nested_block_input_is_rejected() {
        let mut text = String::from("id: deep\nsteps:\n  s:\n    options_override:\n");
        for i in 0..2000 {
            text.push_str(&" ".repeat(6 + i * 2));
            text.push_str(&format!("k{i}:\n"));
        }
        text.push_str(&" ".repeat(6 + 2000 * 2));
        text.push_str("leaf: 1\n");
        assert_rejected("deep block", text);
    }

    #[test]
    fn deeply_nested_flow_input_is_rejected() {
        assert_rejected(
            "deep flow seq",
            format!(
                "id: x\nsteps: {}{}\n",
                "[".repeat(100_000),
                "]".repeat(100_000)
            ),
        );
        assert_rejected(
            "deep flow map",
            format!(
                "id: x\nsteps: {}1{}\n",
                "{a: ".repeat(50_000),
                "}".repeat(50_000)
            ),
        );
        // Unbalanced and deep.
        assert_rejected(
            "deep unbalanced",
            format!("id: x\nsteps: {}\n", "[".repeat(100_000)),
        );
    }

    #[test]
    fn nesting_just_over_the_limit_is_rejected_and_just_under_is_accepted() {
        let nest = |depth: usize| {
            let mut text = String::from("id: edge\nsteps:\n  s:\n    options_override:\n");
            for i in 0..depth {
                text.push_str(&" ".repeat(6 + i * 2));
                text.push_str(&format!("k{i}:\n"));
            }
            text.push_str(&" ".repeat(6 + depth * 2));
            text.push_str("leaf: 1\n");
            text
        };
        // steps -> s -> options_override are 3 levels; 8 more is still far
        // under the limit of 16.
        assert!(parse_on_small_stack(nest(8)).is_ok());
        assert!(parse_on_small_stack(nest(40)).is_err());
    }

    #[test]
    fn over_large_input_is_rejected() {
        // Over the file-size cap: refused before any parsing.
        let big = format!(
            "id: big\ndescription: {}\n",
            "a".repeat(MAX_FOCUS_FILE_BYTES)
        );
        assert_rejected("over the byte cap", big);
        // Under the cap but over the node budget: 70k one-character items.
        let many = format!(
            "id: many\nsuggest_in_focuses:\n{}",
            "  - a\n".repeat(40_000)
        );
        assert!(many.len() < MAX_FOCUS_FILE_BYTES);
        assert_rejected("over the node budget", many);
        // Many keys.
        let keys: String = (0..30_000).map(|i| format!("k{i}: 1\n")).collect();
        assert_rejected("many keys", format!("id: keys\n{keys}"));
        // One very long simple key.
        assert_rejected("long key", format!("id: x\n{}: 1\n", "k".repeat(100_000)));
    }

    #[test]
    fn alias_bombs_and_anchor_floods_are_rejected() {
        // "Billion laughs": each level aliases the previous one many times.
        let mut bomb = String::from("id: bomb\nl0: &l0 [a, a, a, a, a, a, a, a, a]\n");
        for i in 1..9 {
            let prev = format!("*l{}", i - 1);
            let row = vec![prev; 9].join(", ");
            bomb.push_str(&format!("l{i}: &l{i} [{row}]\n"));
        }
        assert_rejected("alias bomb", bomb);
        // More aliases than the limit, even if each is cheap.
        let mut flood = String::from("id: flood\nbase: &b x\nlist:\n");
        flood.push_str(&"  - *b\n".repeat(500));
        assert_rejected("alias flood", flood);
        // More anchors than the limit.
        let anchors: String = (0..500).map(|i| format!("a{i}: &x{i} v\n")).collect();
        assert_rejected("anchor flood", format!("id: anchors\n{anchors}"));
    }

    #[test]
    fn malformed_and_ambiguous_documents_are_rejected() {
        for (label, text) in [
            ("unclosed flow", "id: x\nsteps: [a, b\n"),
            ("unclosed quote", "id: \"never closed\nsteps: {}\n"),
            ("bad indentation", "id: x\nsteps:\n  a: 1\n b: 2\n"),
            ("tab indentation", "id: x\nsteps:\n\ta: 1\n"),
            ("two documents", "id: a\n---\nid: b\n"),
            ("duplicate keys", "id: a\nid: b\n"),
            ("wrong type", "id: x\nmax_routing_tier: [1]\n"),
            ("number too big for u8", "id: x\nmax_routing_tier: 4000\n"),
            ("steps is a scalar", "id: x\nsteps: nope\n"),
            ("undefined alias", "id: x\nguides: *nothing\n"),
            ("control character", "id: x\u{0}y\n"),
            ("only a bare dash", "- \n- \n"),
            ("scalar document", "just text\n"),
            ("tag bomb", "id: !!python/object/apply:os.system [x]\n"),
        ] {
            assert_rejected(label, text.to_owned());
        }
    }

    #[test]
    fn a_rejected_file_never_echoes_its_text() {
        let secret = "SECRET-PROMPT-TEXT-0123456789";
        let r = parse_focus_text(&format!(
            "id: x\nsteps:\n  s:\n    prompt_template: {secret}\n    routing_tier: [oops\n"
        ));
        let msg = r.expect_err("malformed").to_string();
        assert!(!msg.contains(secret), "{msg}");
    }

    #[test]
    fn focus_files_parse_to_the_golden_snapshots() {
        let inputs = focus_parse_inputs();
        assert!(inputs.len() >= 20, "shipped files and fixtures are present");
        for (name, text) in inputs {
            let got = render_focus_snapshot(&parse_focus_text(&text));
            let want = std::fs::read_to_string(focus_parse_golden_dir().join(&name))
                .unwrap_or_else(|e| panic!("missing golden {name}: {e}"));
            assert_eq!(got, want, "parsed result changed for {name}");
        }
    }

    #[test]
    #[ignore]
    fn regenerate_focus_parse_snapshots() {
        for (name, text) in focus_parse_inputs() {
            let got = render_focus_snapshot(&parse_focus_text(&text));
            std::fs::write(focus_parse_golden_dir().join(&name), got).unwrap();
        }
    }

    #[test]
    fn shipped_focus_files_parse_unchanged_after_requires_user_handoff_field_added() {
        // items.id=528 Phase 2: confirms the new Option<bool> RawStep field
        // is genuinely optional to the YAML parser (missing key -> None) against
        // REAL production YAML, not just hand-constructed RawStep literals --
        // and that every shipped step's derivation is unaffected by this
        // change (none use routing_tier: 3, so requires_user_handoff must be
        // false and external_access_override must be Some(..) for all of
        // them, exactly as before this item).
        for filename in [
            "quick-ask.focus",
            "writing-assistant.focus",
            "research-and-buy.focus",
            "role-assessment.focus",
        ] {
            let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../app/core_artifacts/focuses")
                .join(filename);
            let text = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("failed to read {path:?}: {e}"));
            let def = parse_focus_text(&text)
                .unwrap_or_else(|e| panic!("failed to build FocusDefinition for {filename}: {e}"));
            assert!(
                !def.steps.is_empty(),
                "{filename}: expected at least one step"
            );
            for step in &def.steps {
                assert!(
                    !step.requires_user_handoff,
                    "{filename}: step '{}' unexpectedly requires_user_handoff -- \
                     no shipped Focus uses routing_tier: 3",
                    step.action_id
                );
                assert!(
                    step.external_access_override.is_some(),
                    "{filename}: step '{}' has no external_access_override -- \
                     every step here uses routing_tier 1 or 2, never Unrestricted",
                    step.action_id
                );
            }
        }
    }

    #[test]
    fn requires_user_handoff_explicit_false_overrides_legacy_fallback_at_unrestricted() {
        // The YAML field, when present, is authoritative -- it is not just
        // a fallback default. external_access_override stays None regardless
        // (Unrestricted never claims its own capability), but
        // requires_user_handoff itself must reflect the explicit false, not
        // the legacy routing_tier==3 rule.
        let step = parse_single_step_focus(Some(3), Some(false));
        assert!(!step.requires_user_handoff);
        assert_eq!(step.external_access_override, None);
    }

    #[test]
    fn parse_version_defaults_to_1_0() {
        let mut raw = minimal_raw();
        raw.version = None;
        assert_eq!(parse_focus_definition(raw).unwrap().version, "1.0");
    }

    // -------------------------------------------------------------------------
    // output_sensitivity
    // -------------------------------------------------------------------------

    fn run_for_sensitivity(ceiling: i32) -> FocusRun {
        let scheduler = Arc::new(ConductorScheduler::new());
        let mut run = FocusRun::new(
            "u".to_owned(),
            "p".to_owned(),
            "f".to_owned(),
            dummy_pool(),
            scheduler,
            "".to_owned(),
            false,
            None,
            None,
            false,
            std::collections::HashSet::new(),
            None,
        );
        let mut tt = TaskTrack::new();
        tt.add_step(TaskStep {
            action_id: "s".to_owned(),
            output_var: None,
            content: "x".to_owned(),
            sensitivity_severity: ceiling,
            routing_tier_used: 1,
            personal_flagged: false,
            audit_unavailable: false,
        });
        run.task_track = Some(tt);
        run
    }

    #[test]
    fn output_sensitivity_general() {
        assert_eq!(run_for_sensitivity(1).output_sensitivity(), "general");
    }
    #[test]
    fn output_sensitivity_personal() {
        assert_eq!(run_for_sensitivity(2).output_sensitivity(), "personal");
    }
    #[test]
    fn output_sensitivity_medical() {
        assert_eq!(run_for_sensitivity(3).output_sensitivity(), "medical");
    }
    #[test]
    fn output_sensitivity_financial() {
        assert_eq!(run_for_sensitivity(4).output_sensitivity(), "financial");
    }
    #[test]
    fn output_sensitivity_out_of_range_defaults_general() {
        assert_eq!(run_for_sensitivity(99).output_sensitivity(), "general");
    }
    #[test]
    fn output_sensitivity_no_track_defaults_general() {
        let scheduler = Arc::new(ConductorScheduler::new());
        let run: FocusRun = FocusRun::new(
            "u".to_owned(),
            "p".to_owned(),
            "f".to_owned(),
            dummy_pool(),
            scheduler,
            "".to_owned(),
            false,
            None,
            None,
            false,
            std::collections::HashSet::new(),
            None,
        );
        assert_eq!(run.output_sensitivity(), "general");
    }

    /// items.id=710: a run whose attendance nobody states is unattended, so
    /// the spend gate parks it rather than letting it spend beyond an
    /// approved per-run budget. Only the IPC commands a user triggers say
    /// otherwise (commands::execution).
    #[test]
    fn a_new_run_is_unattended_until_a_caller_says_otherwise() {
        let scheduler = Arc::new(ConductorScheduler::new());
        let mut run: FocusRun = FocusRun::new(
            "u".to_owned(),
            "p".to_owned(),
            "f".to_owned(),
            dummy_pool(),
            scheduler,
            "".to_owned(),
            false,
            None,
            None,
            false,
            std::collections::HashSet::new(),
            None,
        );
        assert_eq!(
            run.attendance,
            crate::spend::gate::RunAttendance::Unattended
        );
        run.attendance = crate::spend::gate::RunAttendance::Attended;
        assert_eq!(run.attendance, crate::spend::gate::RunAttendance::Attended);
    }

    /// items.id=715: no run streams unless its owner sets the seam, so every
    /// existing caller keeps whole-body calls.
    #[test]
    fn a_new_run_does_not_stream() {
        let scheduler = Arc::new(ConductorScheduler::new());
        let run: FocusRun = FocusRun::new(
            "u".to_owned(),
            "p".to_owned(),
            "f".to_owned(),
            dummy_pool(),
            scheduler,
            "".to_owned(),
            false,
            None,
            None,
            false,
            std::collections::HashSet::new(),
            None,
        );
        assert!(run.chat_stream.is_none());
    }

    // -------------------------------------------------------------------------
    // Tier computation (pure logic, no DB)
    // -------------------------------------------------------------------------

    #[test]
    fn floor_clamp_at_tier2() {
        let execution_tier: u8 = 2;
        let focus_privacy_tier: u8 = 1;
        let raw = focus_privacy_tier.min(execution_tier);
        let abstraction = if execution_tier > 1 { raw.max(2) } else { raw };
        assert_eq!(raw, 1);
        assert_eq!(abstraction, 2);
    }

    #[test]
    fn no_floor_clamp_at_tier1() {
        let execution_tier: u8 = 1;
        let focus_privacy_tier: u8 = 1;
        let raw = focus_privacy_tier.min(execution_tier);
        let abstraction = if execution_tier > 1 { raw.max(2) } else { raw };
        assert_eq!(raw, 1);
        assert_eq!(abstraction, 1);
    }

    #[test]
    fn floor_clamp_does_not_raise_already_high_tier() {
        let execution_tier: u8 = 2;
        let focus_privacy_tier: u8 = 2;
        let raw = focus_privacy_tier.min(execution_tier);
        let abstraction = if execution_tier > 1 { raw.max(2) } else { raw };
        assert_eq!(raw, 2);
        assert_eq!(abstraction, 2);
    }

    // items.id=444 (Part 7): floor-clamp trigger re-keyed from
    // execution_tier > 1 to effective_access != LocalOnly, where
    // effective_access folds a step's own external_access_override against
    // the Focus-level focus_external_access ceiling.

    #[test]
    fn effective_access_smoke_test_voice_steps_not_floor_clamped() {
        // writing-assistant.focus: voice_analysis/voice_transform declare
        // routing_tier: 1 -> external_access_override: Some(LocalOnly).
        // Focus max_routing_tier: 2 -> AnonymousRequired.
        // Regardless of the user's focus_settings.max_permitted_tier
        // (AnonymousRequired or Unrestricted), these two steps must NOT have
        // their abstraction floor clamped -- confirms items.id=444's fix on
        // the exact shipping Focus that motivated it (spec Part 7d).
        for focus_max_permitted_tier in [
            ExternalAccess::AnonymousRequired,
            ExternalAccess::Unrestricted,
        ] {
            let focus_external_access =
                focus_max_permitted_tier.min(ExternalAccess::AnonymousRequired);
            let step_override = Some(ExternalAccess::LocalOnly);
            let effective_access = step_override
                .map(|o| o.min(focus_external_access))
                .unwrap_or(focus_external_access);
            assert_eq!(effective_access, ExternalAccess::LocalOnly);

            let focus_privacy_tier: u8 = 1;
            let raw_abstraction = focus_privacy_tier.min(1u8); // execution_tier == 1 (step.routing_tier folds it down)
            let abstraction_tier = if effective_access != ExternalAccess::LocalOnly {
                raw_abstraction.max(2)
            } else {
                raw_abstraction
            };
            assert_eq!(
                abstraction_tier, raw_abstraction,
                "voice step must not be floor-clamped at focus_max_permitted_tier={focus_max_permitted_tier:?}"
            );
        }
    }

    #[test]
    fn effective_access_none_override_inherits_focus_level() {
        // A step with no override (e.g. a handoff step) makes no capability
        // claim of its own -- it inherits focus_external_access unchanged,
        // and the ordinary floor clamp still applies.
        let focus_external_access = ExternalAccess::AnonymousRequired;
        let step_override: Option<ExternalAccess> = None;
        let effective_access = step_override
            .map(|o| o.min(focus_external_access))
            .unwrap_or(focus_external_access);
        assert_eq!(effective_access, focus_external_access);

        let focus_privacy_tier: u8 = 1;
        let raw_abstraction = focus_privacy_tier.min(2u8);
        let abstraction_tier = if effective_access != ExternalAccess::LocalOnly {
            raw_abstraction.max(2)
        } else {
            raw_abstraction
        };
        assert_eq!(abstraction_tier, 2);
    }

    // -------------------------------------------------------------------------
    // execution_tier_ordinal (items.id=537: AnonymousPreferred -> 3, fail closed)
    // -------------------------------------------------------------------------

    #[test]
    fn execution_tier_ordinal_anonymous_preferred_matches_unrestricted() {
        // AnonymousPreferred's provider reach equals Unrestricted's, so both
        // must abstract at the same level -- AbstractionLevel::from_tier only
        // distinguishes tier==2 (Level2) from tier>2 (Level3).
        let anonymous_preferred = execution_tier_ordinal(ExternalAccess::AnonymousPreferred);
        let unrestricted = execution_tier_ordinal(ExternalAccess::Unrestricted);
        assert_eq!(anonymous_preferred, unrestricted);
        assert_eq!(anonymous_preferred, 3);
        assert_eq!(
            crate::conductor::privacy::abstraction::AbstractionLevel::from_tier(
                anonymous_preferred
            ),
            crate::conductor::privacy::abstraction::AbstractionLevel::from_tier(unrestricted),
        );
    }

    #[test]
    fn execution_tier_ordinal_pins_against_external_access_order() {
        // Declaration order is ExternalAccess's derive(Ord) order (tokens.rs).
        // execution_tier_ordinal must be non-decreasing across that order,
        // and must NOT collapse AnonymousRequired into AnonymousPreferred/
        // Unrestricted -- only AnonymousPreferred and Unrestricted may tie.
        let ordered = [
            ExternalAccess::LocalOnly,
            ExternalAccess::AnonymousRequired,
            ExternalAccess::AnonymousPreferred,
            ExternalAccess::Unrestricted,
        ];
        let ordinals: Vec<u8> = ordered
            .iter()
            .copied()
            .map(execution_tier_ordinal)
            .collect();
        assert_eq!(ordinals, vec![1, 2, 3, 3]);
        for pair in ordinals.windows(2) {
            assert!(
                pair[0] <= pair[1],
                "ordinal must be non-decreasing: {ordinals:?}"
            );
        }
    }

    #[test]
    fn gate3_promotion_now_fires_anonymous_required_to_anonymous_preferred() {
        // executor.rs's gate3 look-ahead: `next_tier > execution_tier`.
        // Before items.id=537, AnonymousPreferred ordinal was 2, same as
        // AnonymousRequired, so this transition did not cross-tier-promote.
        let execution_tier = execution_tier_ordinal(ExternalAccess::AnonymousRequired);
        let next_tier = execution_tier_ordinal(ExternalAccess::AnonymousPreferred);
        assert!(
            next_tier > execution_tier,
            "AnonymousRequired -> AnonymousPreferred must now trigger gate3 promotion"
        );
    }

    #[test]
    fn gate3_promotion_no_longer_fires_anonymous_preferred_to_unrestricted() {
        // Reach is equal for these two -- nothing widens, so no promotion.
        // Before items.id=537 this pair DID cross-tier-promote (2 -> 3).
        let execution_tier = execution_tier_ordinal(ExternalAccess::AnonymousPreferred);
        let next_tier = execution_tier_ordinal(ExternalAccess::Unrestricted);
        assert!(
            !(next_tier > execution_tier),
            "AnonymousPreferred -> Unrestricted must no longer trigger gate3 promotion"
        );
    }

    // -------------------------------------------------------------------------
    // apply_entity_fact_provenance_check (decisions.id=546, decisions.id=424,
    // items.id=27)
    // -------------------------------------------------------------------------

    fn run_with_persona(persona_id: &str) -> FocusRun {
        run_with_persona_and_confirmed(persona_id, std::collections::HashSet::new())
    }

    fn run_with_persona_and_confirmed(
        persona_id: &str,
        confirmed_cross_persona_fact_ids: std::collections::HashSet<String>,
    ) -> FocusRun {
        let scheduler = Arc::new(ConductorScheduler::new());
        FocusRun::new(
            "u".to_owned(),
            persona_id.to_owned(),
            "f".to_owned(),
            dummy_pool(),
            scheduler,
            "".to_owned(),
            false,
            None,
            None,
            false,
            confirmed_cross_persona_fact_ids,
            None,
        )
    }

    fn make_fact(
        id: &str,
        source_persona_id: &str,
        cross_persona_export: bool,
        origin_persona_id: Option<&str>,
    ) -> crate::conductor::types::EntityFact {
        crate::conductor::types::EntityFact {
            id: id.to_owned(),
            entity_id: None,
            field_name: "gpa".to_owned(),
            field_value: "3.8".to_owned(),
            sensitivity: "personal".to_owned(),
            sensitivity_severity: 2,
            source_persona_id: source_persona_id.to_owned(),
            cross_persona_export,
            origin_persona_id: origin_persona_id.map(|s| s.to_owned()),
        }
    }

    #[tokio::test]
    async fn provenance_same_persona_includes_normally() {
        let run = run_with_persona("persona-student");
        let mut track = PersonalTrack::new();
        let fact = make_fact("fact-1", "persona-student", false, None);

        let result = run
            .apply_entity_fact_provenance_check(&mut track, fact)
            .await;

        assert!(result.is_ok());
        assert_eq!(track.entity_facts().len(), 1);
    }

    #[tokio::test]
    async fn provenance_mismatched_persona_no_export_is_hard_block() {
        let run = run_with_persona("persona-work");
        let mut track = PersonalTrack::new();
        // source_persona_id != run persona, cross_persona_export = false —
        // the "should be unreachable" integrity-violation case.
        let fact = make_fact("fact-1", "persona-student", false, None);

        let result = run
            .apply_entity_fact_provenance_check(&mut track, fact)
            .await;

        assert!(result.is_err());
        assert_eq!(track.entity_facts().len(), 0);
        match result.unwrap_err() {
            LifecycleError::ProvenanceIntegrityViolation(msg) => {
                assert!(msg.contains("integrity violation"));
            }
            other => panic!("expected ProvenanceIntegrityViolation, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn provenance_cross_persona_export_confirmed_includes() {
        // decisions.id=639: fact.id present in confirmed_cross_persona_fact_ids
        // (the pre-Focus-start IPC confirmation result) — include.
        let mut confirmed = std::collections::HashSet::new();
        confirmed.insert("fact-1".to_owned());
        let run = run_with_persona_and_confirmed("persona-work", confirmed);
        let mut track = PersonalTrack::new();
        let fact = make_fact("fact-1", "persona-student", true, Some("persona-student"));

        let result = run
            .apply_entity_fact_provenance_check(&mut track, fact)
            .await;

        assert!(result.is_ok());
        assert_eq!(track.entity_facts().len(), 1);
        // decisions.id=815: a confirmed/included fact is not an omission —
        // must not show up in the RunStatusPayload.provenance_omissions
        // source either.
        assert_eq!(track.omitted_cross_persona_facts().len(), 0);
    }

    #[tokio::test]
    async fn provenance_cross_persona_export_declined_omits_not_blocks() {
        // decisions.id=639 + Jason (items.id=27 session 2026-07-25): fact.id
        // NOT in confirmed_cross_persona_fact_ids — declined (or never
        // presented). Must OMIT from context, not hard-block the run.
        let run = run_with_persona("persona-work"); // empty confirmed set
        let mut track = PersonalTrack::new();
        let fact = make_fact("fact-1", "persona-student", true, Some("persona-student"));

        let result = run
            .apply_entity_fact_provenance_check(&mut track, fact)
            .await;

        assert!(
            result.is_ok(),
            "declined cross-Persona fact must not error the run"
        );
        assert_eq!(
            track.entity_facts().len(),
            0,
            "declined fact must be omitted, not included"
        );
        // decisions.id=815, items.id=27: the omission must also be recorded
        // on the track, for RunStatusPayload.provenance_omissions to surface
        // it as a live signal (previously only a disclosure_log row).
        let omitted = track.omitted_cross_persona_facts();
        assert_eq!(omitted.len(), 1);
        assert_eq!(omitted[0].field_name, "gpa");
        assert_eq!(omitted[0].sensitivity, "personal");
        assert_eq!(
            omitted[0].origin_persona_id.as_deref(),
            Some("persona-student")
        );
    }

    #[tokio::test]
    async fn provenance_cross_persona_export_confirmed_id_only_matches_exact_fact() {
        // A confirmed id for a DIFFERENT fact must not accidentally confirm
        // this one — confirmation is per-fact-id, not persona-wide.
        let mut confirmed = std::collections::HashSet::new();
        confirmed.insert("some-other-fact".to_owned());
        let run = run_with_persona_and_confirmed("persona-work", confirmed);
        let mut track = PersonalTrack::new();
        let fact = make_fact("fact-1", "persona-student", true, Some("persona-student"));

        let result = run
            .apply_entity_fact_provenance_check(&mut track, fact)
            .await;

        assert!(result.is_ok());
        assert_eq!(track.entity_facts().len(), 0);
    }

    #[tokio::test]
    async fn provenance_check_does_not_affect_personal_fields() {
        // The provenance check operates on entity_facts only — must never
        // touch track.fields() (the separate personal_fields store).
        let run = run_with_persona("persona-student");
        let mut track = PersonalTrack::new();
        let fact = make_fact("fact-1", "persona-student", false, None);

        run.apply_entity_fact_provenance_check(&mut track, fact)
            .await
            .unwrap();

        assert_eq!(track.fields().len(), 0);
        assert_eq!(track.entity_facts().len(), 1);
    }

    // -------------------------------------------------------------------------
    // load_group_facts_into_track (items.id=296)
    //
    // KNOWN TEST BOUNDARY: FocusRun.app_handle is concretely typed
    // Option<tauri::AppHandle<tauri::Wry>> (not generic over the runtime),
    // and tauri::test::mock_app() only ever produces an
    // AppHandle<tauri::test::MockRuntime> -- there is no way to construct a
    // real AppHandle<Wry> in a unit test, which is exactly why every other
    // FocusRun test helper in this file (run_with_persona and friends)
    // passes app_handle: None. That is a pre-existing limitation of this
    // test infrastructure, not something introduced here. What IS tested
    // directly here is the graceful no-op when app_handle is None (the
    // state every unit test — including every other test in this file —
    // actually runs FocusRun in). The opted-in/resident-key gating logic
    // itself is covered at the layer below: persistence::
    // group_fact_sources_store's own tests (opt-in CRUD),
    // persistence::group_fact_store's own tests (group.db read), and
    // auth::registry::GroupKeyRegistry's own tests (key_hex_for
    // resident/absent behavior) — load_group_facts_into_track is a thin
    // composition of exactly those three already-tested primitives.
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn load_group_facts_into_track_is_a_noop_without_an_app_handle() {
        let run = run_with_persona("persona-a"); // app_handle: None
        let mut track = PersonalTrack::new();

        let result = run.load_group_facts_into_track(&mut track, "").await;

        assert!(result.is_ok(), "must not error when app_handle is absent");
        assert_eq!(
            track.group_facts().len(),
            0,
            "no group facts can be loaded without an AppHandle to resolve GroupKeyRegistry from"
        );
    }

    // -------------------------------------------------------------------------
    // Disclosure-log write path (items.id=173, "Layer 8") — previously
    // untestable per the KNOWN LIMITATIONS note this genericization removed.
    // FocusRun<TestLogger> injects a TestLogger so the entry shape written
    // by the cross-Persona omission path (above) can be asserted directly,
    // rather than only the omit BEHAVIOUR (track.entity_facts().len()).
    // -------------------------------------------------------------------------

    fn run_with_persona_and_test_logger(
        persona_id: &str,
    ) -> FocusRun<crate::conductor::privacy::logger::TestLogger> {
        let scheduler = Arc::new(ConductorScheduler::new());
        let mut run: FocusRun<crate::conductor::privacy::logger::TestLogger> = FocusRun::new(
            "u".to_owned(),
            persona_id.to_owned(),
            "f".to_owned(),
            dummy_pool(),
            scheduler,
            "".to_owned(),
            false,
            None,
            None,
            false,
            std::collections::HashSet::new(),
            None,
        );
        // apply_entity_fact_provenance_check only writes when
        // self.privacy_gateway is Some (see its `if let Some(gateway)`
        // guard) — these fixtures never call initialize() (Phase 3), so the
        // gateway must be constructed directly here, matching initialize()'s
        // own L::for_run(...) call for parity with production wiring.
        use crate::conductor::privacy::logger::DisclosureLoggerForRun;
        use crate::conductor::privacy::PrivacyGateway;
        run.privacy_gateway = Some(PrivacyGateway::new(
            crate::conductor::privacy::logger::TestLogger::for_run("u", persona_id, ""),
        ));
        run
    }

    #[tokio::test]
    async fn provenance_declined_cross_persona_fact_writes_disclosure_log_entry() {
        let run = run_with_persona_and_test_logger("persona-work");
        let mut track = PersonalTrack::new();
        let fact = make_fact("fact-1", "persona-student", true, Some("persona-student"));

        run.apply_entity_fact_provenance_check(&mut track, fact)
            .await
            .unwrap();

        let logger = &run.privacy_gateway.as_ref().unwrap().logger;
        assert_eq!(
            logger.entry_count(),
            1,
            "the decline must produce exactly one disclosure-log entry"
        );
        let entry = &logger.entries()[0];
        assert_eq!(entry.action_id, "initialize");
        // Key format is "entity_id:field_name" (compute_content_hash's own
        // convention, per the surrounding source comment) — NOT the fact's
        // own id. make_fact() gives this fact entity_id=None and
        // field_name="gpa", so the withheld key is ":gpa".
        assert_eq!(
            entry.fields_withheld,
            vec![":gpa".to_owned()],
            "withheld entry must be keyed entity_id:field_name"
        );
    }

    #[tokio::test]
    async fn provenance_confirmed_cross_persona_fact_writes_no_disclosure_log_entry() {
        // The confirmed (included) path does not go through the omission
        // branch at all — only a decline produces a disclosure-log entry
        // here. Confirms the write path is decline-specific, not fired on
        // every provenance check.
        let mut confirmed = std::collections::HashSet::new();
        confirmed.insert("fact-1".to_owned());
        let scheduler = Arc::new(ConductorScheduler::new());
        let mut run: FocusRun<crate::conductor::privacy::logger::TestLogger> = FocusRun::new(
            "u".to_owned(),
            "persona-work".to_owned(),
            "f".to_owned(),
            dummy_pool(),
            scheduler,
            "".to_owned(),
            false,
            None,
            None,
            false,
            confirmed,
            None,
        );
        use crate::conductor::privacy::logger::DisclosureLoggerForRun;
        use crate::conductor::privacy::PrivacyGateway;
        run.privacy_gateway = Some(PrivacyGateway::new(
            crate::conductor::privacy::logger::TestLogger::for_run("u", "persona-work", ""),
        ));
        let mut track = PersonalTrack::new();
        let fact = make_fact("fact-1", "persona-student", true, Some("persona-student"));

        run.apply_entity_fact_provenance_check(&mut track, fact)
            .await
            .unwrap();

        let logger = &run.privacy_gateway.as_ref().unwrap().logger;
        assert_eq!(
            logger.entry_count(),
            0,
            "a confirmed (included) fact must not write a disclosure-log entry"
        );
    }

    // -------------------------------------------------------------------------
    // R1 crisis-handling floor -- cloud_frontier boundary and handle_step_failure()
    // (items.id=297). Both are Phase 4 EXECUTE exits that leave a run
    // without ever reaching output() (the pre-existing, already-tested-by-
    // hand crisis-append point), so RunResult.crisis_resource_block must be
    // populated here identically to how output() populates it.
    // -------------------------------------------------------------------------

    const CRISIS_TEST_KEY_HEX: &str =
        "1122334455667788112233445566778811223344556677881122334455667788";

    /// Run a future with QR_DATA_ROOT pointed at a scratch tempdir, matching
    /// document_fork_store.rs's established per-test sandboxing pattern --
    /// write_checkpoint()/write_focus_run_record() do real (non-fatal-on-
    /// error, but real) DB I/O even in these tests, and must never touch a
    /// real user data directory.
    async fn with_temp_data_root<F: std::future::Future>(f: F) -> F::Output {
        let _lock = crate::test_support::ENV_MUTEX.lock().await;
        let saved_root = std::env::var("QR_DATA_ROOT").ok();
        let tempdir = tempfile::tempdir().expect("failed to create tempdir");
        std::env::set_var("QR_DATA_ROOT", tempdir.path());

        let result = f.await;

        if let Some(v) = saved_root {
            std::env::set_var("QR_DATA_ROOT", v);
        } else {
            std::env::remove_var("QR_DATA_ROOT");
        }
        result
    }

    /// A FocusDefinition with a single cloud_frontier step -- enough for execute()'s
    /// loop to hit the cloud_frontier boundary on its very first iteration, before
    /// execute_step() (and everything it would otherwise require) ever runs.
    fn single_tier3_step_focus_def() -> FocusDefinition {
        let mut steps_map = IndexMap::new();
        steps_map.insert(
            "step_a".to_owned(),
            RawStep {
                display_name: Some("Step A".to_owned()),
                guide_id: None,
                task_type: Some("general".to_owned()),
                routing_tier: Some(3),
                requires_user_handoff: None,
                step_type: None,
                output_var: Some("result".to_owned()),
                prompt_template: Some("Hello {user_input}".to_owned()),
                field_requirements: None,
                options_override: None,
                declared_inputs: None,
            },
        );
        let mut raw = minimal_raw();
        raw.max_routing_tier = Some(3);
        raw.steps = Some(steps_map);
        parse_focus_definition(raw).unwrap()
    }

    /// A FocusRun with every field execute()'s own assert!()s require
    /// populated (sealed PersonalTrack, task_track, shared_state, focus_def,
    /// focus_run_id, failure_handler, privacy_gateway) -- everything pure/
    /// in-memory except the DB writes execute() itself triggers, which is
    /// what with_temp_data_root() above is for. app_handle stays None (as
    /// in every other test in this file), so emit_status_with_content() is a
    /// silent no-op here -- these tests assert on the returned RunResult,
    /// not on the push event.
    fn tier3_ready_run(
        crisis_floor_triggered: bool,
    ) -> FocusRun<crate::conductor::privacy::logger::TestLogger> {
        use crate::conductor::privacy::logger::DisclosureLoggerForRun;

        let scheduler = Arc::new(ConductorScheduler::new());
        let mut run: FocusRun<crate::conductor::privacy::logger::TestLogger> = FocusRun::new(
            "u".to_owned(),
            "p".to_owned(),
            "f".to_owned(),
            dummy_pool(),
            scheduler,
            "".to_owned(),
            false,
            Some(CRISIS_TEST_KEY_HEX.to_owned()),
            None,
            false,
            std::collections::HashSet::new(),
            None,
        );
        run.focus_run_id = Some(uuid::Uuid::new_v4().to_string());
        run.focus_def = Some(single_tier3_step_focus_def());
        let mut personal_track = PersonalTrack::new();
        personal_track.seal();
        run.personal_track = Some(personal_track);
        run.task_track = Some(TaskTrack::new());
        run.shared_state = Some(SharedStateTrack::new());
        run.failure_handler = Some(FailureHandler::new(ExternalAccess::LocalOnly));
        run.privacy_gateway = Some(PrivacyGateway::new(
            crate::conductor::privacy::logger::TestLogger::for_run("u", "p", ""),
        ));
        run.crisis_floor_triggered = crisis_floor_triggered;
        run
    }

    #[tokio::test]
    async fn tier3_boundary_sets_crisis_resource_block_when_triggered() {
        with_temp_data_root(async {
            let mut run = tier3_ready_run(true);
            let result = run
                .execute()
                .await
                .unwrap()
                .expect("a Tier 3 first step must return Some(RunResult), not fall through");
            assert_eq!(result.status, "awaiting_user");
            let block = result
                .crisis_resource_block
                .expect("crisis-flagged Tier 3 pause must carry the resource block");
            assert!(
                block.contains("988"),
                "must be the real crisis resource text, not a placeholder: {block}"
            );
        })
        .await;
    }

    #[tokio::test]
    async fn tier3_boundary_omits_crisis_resource_block_when_not_triggered() {
        with_temp_data_root(async {
            let mut run = tier3_ready_run(false);
            let result = run
                .execute()
                .await
                .unwrap()
                .expect("a Tier 3 first step must return Some(RunResult), not fall through");
            assert_eq!(result.status, "awaiting_user");
            assert!(
                result.crisis_resource_block.is_none(),
                "a non-crisis-flagged run must not get the resource block"
            );
        })
        .await;
    }

    /// items.id=317: the cloud_frontier pause must carry the prior step's real
    /// output as RunResult.output_content -- this is the only place the
    /// draft awaiting Gate3 review (retired per-reply review, items.id=501 slice 3)
    /// ever gets captured, since execute() returns before reaching output()
    /// on this path and output_store is never written to for this run.
    #[tokio::test]
    async fn tier3_boundary_carries_prior_step_output_as_draft_content() {
        with_temp_data_root(async {
            let mut run = tier3_ready_run(false);
            run.task_track.as_mut().unwrap().add_step(TaskStep {
                action_id: "step_prior".to_owned(),
                output_var: None,
                content: "the draft awaiting gate3 review".to_owned(),
                sensitivity_severity: 1,
                routing_tier_used: 1,
                personal_flagged: false,
                audit_unavailable: false,
            });
            let result = run
                .execute()
                .await
                .unwrap()
                .expect("a Tier 3 first step must return Some(RunResult), not fall through");
            assert_eq!(result.status, "awaiting_user");
            assert_eq!(
                result.output_content.as_deref(),
                Some("the draft awaiting gate3 review"),
                "output_content must be the prior step's real content, not None"
            );
        })
        .await;
    }

    /// Symmetric to the above: with no prior step (single_tier3_step_focus_def's
    /// cloud_frontier step is the very first step), task_track.last_output() is
    /// legitimately None -- output_content must stay None too, not panic or
    /// substitute a placeholder.
    #[tokio::test]
    async fn tier3_boundary_omits_draft_content_when_no_prior_step_ran() {
        with_temp_data_root(async {
            let mut run = tier3_ready_run(false);
            let result = run
                .execute()
                .await
                .unwrap()
                .expect("a Tier 3 first step must return Some(RunResult), not fall through");
            assert_eq!(result.status, "awaiting_user");
            assert!(
                result.output_content.is_none(),
                "no prior step ran, so there is no draft to carry"
            );
        })
        .await;
    }

    /// Minimal FocusRun for handle_step_failure() directly -- unlike the
    /// cloud_frontier boundary, handle_step_failure() takes its FailureResult as a
    /// plain parameter rather than deriving it from self.failure_handler /
    /// self.privacy_gateway / a real step, so it needs none of those: just
    /// focus_run_id + key_hex (write_focus_run_record() panics without
    /// either) and the crisis flag under test.
    ///
    /// personal_track/task_track/shared_state ARE required, though (items.id=245):
    /// the awaiting_user branch now calls write_checkpoint(), which unwrap()s
    /// all three -- unlike execute()'s own callers of write_checkpoint(),
    /// which get there only after execute()'s top-level assert!()s already
    /// guarantee these are Some, a test that calls handle_step_failure()
    /// directly bypasses that guarantee and must supply it here instead.
    fn failure_ready_run(crisis_floor_triggered: bool) -> FocusRun {
        let scheduler = Arc::new(ConductorScheduler::new());
        let mut run: FocusRun = FocusRun::new(
            "u".to_owned(),
            "p".to_owned(),
            "f".to_owned(),
            dummy_pool(),
            scheduler,
            "".to_owned(),
            false,
            Some(CRISIS_TEST_KEY_HEX.to_owned()),
            None,
            false,
            std::collections::HashSet::new(),
            None,
        );
        run.focus_run_id = Some(uuid::Uuid::new_v4().to_string());
        run.crisis_floor_triggered = crisis_floor_triggered;
        let mut personal_track = PersonalTrack::new();
        personal_track.seal();
        run.personal_track = Some(personal_track);
        run.task_track = Some(TaskTrack::new());
        run.shared_state = Some(SharedStateTrack::new());
        run
    }

    fn stop_failure() -> FailureResult {
        FailureResult {
            action: FailureAction::Stop,
            failure_mode: Some("F1".to_owned()),
            plain_language: "Something went wrong.".to_owned(),
            is_recoverable: false,
            severity: FailureSeverity::Stop,
            action_id: Some("step_a".to_owned()),
            focus_id: Some("f".to_owned()),
            metadata: None,
        }
    }

    fn await_user_failure() -> FailureResult {
        FailureResult {
            action: FailureAction::AwaitFloorConsent,
            failure_mode: Some("F6".to_owned()),
            plain_language: "Needs your input to continue.".to_owned(),
            is_recoverable: true,
            severity: FailureSeverity::Pause,
            action_id: Some("step_a".to_owned()),
            focus_id: Some("f".to_owned()),
            metadata: None,
        }
    }

    #[tokio::test]
    async fn handle_step_failure_stop_branch_sets_crisis_resource_block_when_triggered() {
        with_temp_data_root(async {
            let mut run = failure_ready_run(true);
            let result = run.handle_step_failure(stop_failure()).await.unwrap();
            assert_eq!(result.status, "failed");
            let block = result
                .crisis_resource_block
                .expect("crisis-flagged Stop failure must carry the resource block");
            assert!(block.contains("988"));
        })
        .await;
    }

    #[tokio::test]
    async fn handle_step_failure_await_user_branch_sets_crisis_resource_block_when_triggered() {
        with_temp_data_root(async {
            let mut run = failure_ready_run(true);
            let result = run.handle_step_failure(await_user_failure()).await.unwrap();
            // Round-trips through write_focus_run_record()'s INSERT and
            // get_current_status()'s own SELECT against the same tempdir
            // outputs.db -- confirms this test's fixture is exercising the
            // real DB path, not silently short-circuiting on an error.
            assert_eq!(result.status, "awaiting_user");
            let block = result
                .crisis_resource_block
                .expect("crisis-flagged AwaitFloorConsent pause must carry the resource block");
            assert!(block.contains("988"));
        })
        .await;
    }

    #[tokio::test]
    async fn handle_step_failure_omits_crisis_resource_block_when_not_triggered() {
        with_temp_data_root(async {
            let mut run = failure_ready_run(false);
            let stop_result = run.handle_step_failure(stop_failure()).await.unwrap();
            assert!(stop_result.crisis_resource_block.is_none());

            let mut run2 = failure_ready_run(false);
            let await_result = run2
                .handle_step_failure(await_user_failure())
                .await
                .unwrap();
            assert!(
                await_result.crisis_resource_block.is_none(),
                "neither branch should populate the block for a non-crisis-flagged run"
            );
        })
        .await;
    }

    #[tokio::test]
    async fn handle_step_failure_await_user_branch_checkpoints_the_paused_step() {
        // items.id=245: without this checkpoint, rehydrate_focus_run()'s
        // status=='awaiting_user' branch (which resumes AT the last-checkpointed
        // step) would resume at a stale earlier checkpoint -- or find none at
        // all -- instead of the step that actually paused.
        with_temp_data_root(async {
            let mut run = failure_ready_run(false);
            let focus_run_id = run.focus_run_id.clone().unwrap();

            // Seed the focus_runs row write_checkpoint()'s FK
            // (focus_run_snapshots.focus_run_id REFERENCES focus_runs(id),
            // enforced -- connect_options_encrypted sets foreign_keys(true))
            // requires. In production this row always exists already (AUTHORIZE
            // writes it before EXECUTE ever starts); failure_ready_run() skips
            // AUTHORIZE, so this test must seed it directly instead.
            crate::persistence::output_store::test_seed_focus_run(
                &run.user_id,
                &run.persona_id,
                CRISIS_TEST_KEY_HEX,
                &focus_run_id,
                "f",
            )
            .await
            .expect("test_seed_focus_run must succeed");

            run.handle_step_failure(await_user_failure()).await.unwrap();

            let mut conn = open_outputs_db(&run.user_id, &run.persona_id, CRISIS_TEST_KEY_HEX)
                .await
                .expect("open_outputs_db must succeed");
            let row =
                sqlx::query("SELECT action_id FROM focus_run_snapshots WHERE focus_run_id = ?")
                    .bind(&focus_run_id)
                    .fetch_optional(&mut conn)
                    .await
                    .expect("snapshot query must succeed");
            let row = row.expect(
                "handle_step_failure's awaiting_user branch must write a checkpoint \
                 for the paused step",
            );
            let action_id: String = row.try_get("action_id").unwrap();
            assert_eq!(
                action_id, "step_a",
                "checkpoint must be keyed to the step that actually paused \
                 (await_user_failure()'s FailureResult.action_id), not some other step"
            );
        })
        .await;
    }

    // -------------------------------------------------------------------------
    // rehydrate_focus_run / resume_execution (items.id=245, building on the
    // items.id=574 Part 1 foundation: focus_runs.user_input + rehydrate_focus_run()).
    // -------------------------------------------------------------------------

    const RESUME_TEST_KEY_HEX: &str =
        "3344556677889900334455667788990033445566778899003344556677889900";

    /// Real shared.db, migrated, with a persona for "quick-ask" --
    /// rehydrate_focus_run()'s resolve_tier_config() call requires both the
    /// persona and a focus_settings row to exist (same precondition
    /// authorize() has always had). No explicit focus_settings insert here:
    /// create_persona() below already seeds one for "quick-ask" (one of
    /// persona_store::SEEDED_FOCUS_IDS) for every persona it creates.
    /// Mirrors commands::persona's own setup() (persona.rs:519-569).
    async fn setup_shared_and_persona_for_resume_test(
        user_id: &str,
        persona_id: &str,
    ) -> sqlx::SqlitePool {
        crate::persistence::migrations::migrate_shared_db()
            .await
            .expect("shared.db migration must succeed in test setup");

        let pool =
            sqlx::SqlitePool::connect_with(crate::providers::utils::connect_options_unencrypted(
                &crate::providers::utils::db_path_shared(),
            ))
            .await
            .expect("shared.db pool must connect");

        crate::auth::user_store::create_user(
            &pool,
            user_id,
            "Resume Test User",
            "user",
            false,
            &[0u8; crate::auth::kdf::SALT_LEN],
            crate::auth::kdf::DEFAULT_ARGON2_MEMORY_KIB,
            crate::auth::kdf::DEFAULT_ARGON2_ITERATIONS,
            crate::auth::kdf::DEFAULT_ARGON2_PARALLELISM,
            &[0u8; 32],
        )
        .await
        .expect("create_user must succeed in test setup");

        crate::persistence::persona_store::create_persona_anchor_for_test(
            &pool, persona_id, user_id,
        )
        .await
        .expect("create_persona must succeed in test setup");

        pool
    }

    // -------------------------------------------------------------------------
    // items.id=735 (decisions.id=883): Gate 2 flags on the TaskStep travel to
    // the step-completion status and through checkpoint and resume.
    // -------------------------------------------------------------------------

    fn flagged_step(severity: i32, audit_unavailable: bool) -> TaskStep {
        TaskStep {
            action_id: "draft".to_owned(),
            output_var: Some("draft_output".to_owned()),
            content: "Reply that echoed a withheld value".to_owned(),
            sensitivity_severity: severity,
            routing_tier_used: 2,
            personal_flagged: true,
            audit_unavailable,
        }
    }

    #[test]
    fn an_old_checkpoint_without_the_gate_2_fields_deserializes_with_both_false() {
        let json = r#"{"steps":[{"action_id":"s1","output_var":"o","content":"c",
            "sensitivity_severity":3,"routing_tier_used":2}],"sensitivity_ceiling":3}"#;
        let track = deserialize_task_track(json).unwrap();
        let step = &track.steps()[0];
        assert!(!step.personal_flagged && !step.audit_unavailable);
        assert_eq!(track.sensitivity_ceiling(), 3);
    }

    #[test]
    fn replaying_flagged_steps_restores_the_flags_and_the_raised_ceiling() {
        let json = r#"{"steps":[
            {"action_id":"s1","output_var":"a","content":"c1","sensitivity_severity":1,
             "routing_tier_used":2,"personal_flagged":false,"audit_unavailable":false},
            {"action_id":"s2","output_var":"b","content":"c2","sensitivity_severity":4,
             "routing_tier_used":2,"personal_flagged":true,"audit_unavailable":true}
          ],"sensitivity_ceiling":4}"#;
        let track = deserialize_task_track(json).unwrap();
        assert!(!track.steps()[0].personal_flagged);
        assert!(track.steps()[1].personal_flagged && track.steps()[1].audit_unavailable);
        assert_eq!(track.sensitivity_ceiling(), 4);
    }

    #[test]
    fn the_step_completion_status_carries_the_flags_only_for_a_flagged_step() {
        let mut run = failure_ready_run(false);
        // A clean step: both absent.
        run.task_track.as_mut().unwrap().add_step(TaskStep {
            personal_flagged: false,
            ..flagged_step(1, false)
        });
        let flags = run.last_step_reply_flags();
        assert_eq!(flags, None);
        let p = run.build_status_payload("running", Some("Draft"), Some("c"), None, flags);
        assert_eq!((p.personal_flagged, p.audit_unavailable), (None, None));

        // A flagged step with the audit entry written.
        run.task_track
            .as_mut()
            .unwrap()
            .add_step(flagged_step(3, false));
        let flags = run.last_step_reply_flags();
        assert_eq!(flags, Some(false));
        let p = run.build_status_payload("running", Some("Draft"), Some("c"), None, flags);
        assert_eq!(
            (p.personal_flagged, p.audit_unavailable),
            (Some(true), None)
        );

        // A flagged step whose audit entry could not be written.
        run.task_track
            .as_mut()
            .unwrap()
            .add_step(flagged_step(3, true));
        let flags = run.last_step_reply_flags();
        let p = run.build_status_payload("running", Some("Draft"), Some("c"), None, flags);
        assert_eq!(
            (p.personal_flagged, p.audit_unavailable),
            (Some(true), Some(true))
        );
        assert_eq!(p.step_content.as_deref(), Some("c"));
    }

    #[test]
    fn failed_and_awaiting_user_statuses_never_carry_the_flags() {
        let mut run = failure_ready_run(false);
        run.task_track
            .as_mut()
            .unwrap()
            .add_step(flagged_step(4, true));
        // emit_status_with_content (the failed / awaiting_user / draft path)
        // passes no flags whatever the last step recorded.
        for status in ["failed", "awaiting_user", "awaiting_feedback"] {
            let p = run.build_status_payload(status, None, None, None, None);
            assert_eq!((p.personal_flagged, p.audit_unavailable), (None, None));
        }
    }

    #[tokio::test]
    async fn resume_replays_the_gate_2_flags_and_the_raised_ceiling() {
        let _lock = crate::test_support::ENV_MUTEX.lock().await;
        let saved_root = std::env::var("QR_DATA_ROOT").ok();
        let tempdir = tempfile::tempdir().expect("failed to create tempdir");
        std::env::set_var("QR_DATA_ROOT", tempdir.path());

        let user_id = "flag-resume-user";
        let persona_id = "flag-resume-persona";
        let pool = setup_shared_and_persona_for_resume_test(user_id, persona_id).await;
        let focus_run_id = uuid::Uuid::new_v4().to_string();
        {
            let mut conn = open_outputs_db(user_id, persona_id, RESUME_TEST_KEY_HEX)
                .await
                .unwrap();
            sqlx::query(
                "INSERT INTO focus_runs
                 (id, focus_id, status, is_fast_lane, is_quick_ask, topic_id, user_input, started_at)
                 VALUES (?, 'quick-ask', 'paused', 0, 1, NULL, ?, ?)",
            )
            .bind(&focus_run_id)
            .bind("input")
            .bind(now())
            .execute(&mut conn)
            .await
            .unwrap();
        }
        {
            let scheduler = Arc::new(ConductorScheduler::new());
            let mut seed_run: FocusRun = FocusRun::new(
                user_id.to_owned(),
                persona_id.to_owned(),
                "quick-ask".to_owned(),
                pool.clone(),
                scheduler,
                "input".to_owned(),
                false,
                Some(RESUME_TEST_KEY_HEX.to_owned()),
                None,
                true,
                std::collections::HashSet::new(),
                None,
            );
            seed_run.focus_run_id = Some(focus_run_id.clone());
            let mut personal_track = PersonalTrack::new();
            personal_track.seal();
            seed_run.personal_track = Some(personal_track);
            let mut task_track = TaskTrack::new();
            task_track.add_step(flagged_step(4, true));
            seed_run.task_track = Some(task_track);
            seed_run.shared_state = Some(SharedStateTrack::new());
            seed_run.write_checkpoint("draft").await.unwrap();
        }

        let run: FocusRun = rehydrate_focus_run(
            user_id.to_owned(),
            persona_id.to_owned(),
            focus_run_id.clone(),
            pool.clone(),
            Arc::new(ConductorScheduler::new()),
            Some(RESUME_TEST_KEY_HEX.to_owned()),
            std::collections::HashSet::new(),
            None,
        )
        .await
        .expect("rehydrate_focus_run must succeed");

        let track = run.task_track.as_ref().unwrap();
        let step = &track.steps()[0];
        assert!(step.personal_flagged && step.audit_unavailable);
        assert_eq!(step.sensitivity_severity, 4);
        assert_eq!(track.sensitivity_ceiling(), 4);
        assert_eq!(run.last_step_reply_flags(), Some(true));

        match saved_root {
            Some(v) => std::env::set_var("QR_DATA_ROOT", v),
            None => std::env::remove_var("QR_DATA_ROOT"),
        }
    }

    // -------------------------------------------------------------------------
    // items.id=716: a rehydrated api-chat run goes back to its bound destination
    // -------------------------------------------------------------------------

    /// Seeds a paused api-chat run (focus_runs row in outputs.db) and returns
    /// its id. The Persona's api-chat settings row is made at the given ceiling.
    async fn seed_paused_api_chat_run(
        pool: &sqlx::SqlitePool,
        user_id: &str,
        persona_id: &str,
    ) -> String {
        crate::persistence::focus_settings_store::ensure_focus_settings_default(
            pool,
            persona_id,
            crate::api_chat::API_CHAT_FOCUS_ID,
            ExternalAccess::Unrestricted,
        )
        .await
        .expect("api-chat settings row");
        let run_id = uuid::Uuid::new_v4().to_string();
        let mut conn = open_outputs_db(user_id, persona_id, RESUME_TEST_KEY_HEX)
            .await
            .expect("open_outputs_db");
        sqlx::query(
            "INSERT INTO focus_runs
             (id, focus_id, status, is_fast_lane, is_quick_ask, topic_id, user_input, started_at)
             VALUES (?, 'api-chat', 'awaiting_user', 0, 0, NULL, 'User: hi', ?)",
        )
        .bind(&run_id)
        .bind(now())
        .execute(&mut conn)
        .await
        .expect("seed focus_runs");
        run_id
    }

    async fn rehydrate_api_chat(
        pool: &sqlx::SqlitePool,
        user_id: &str,
        persona_id: &str,
        run_id: &str,
    ) -> Result<FocusRun, LifecycleError> {
        rehydrate_focus_run(
            user_id.to_owned(),
            persona_id.to_owned(),
            run_id.to_owned(),
            pool.clone(),
            Arc::new(ConductorScheduler::new()),
            Some(RESUME_TEST_KEY_HEX.to_owned()),
            std::collections::HashSet::new(),
            None,
        )
        .await
    }

    #[tokio::test]
    async fn a_rehydrated_api_chat_run_re_derives_its_binding_through_the_reply_message() {
        use crate::persistence::chat_binding_store::{create_binding, ChatKind};
        let _lock = crate::test_support::ENV_MUTEX.lock().await;
        let saved_root = std::env::var("QR_DATA_ROOT").ok();
        let tempdir = tempfile::tempdir().expect("tempdir");
        std::env::set_var("QR_DATA_ROOT", tempdir.path());

        let (user_id, persona_id) = ("bind-user", "bind-persona");
        let pool = setup_shared_and_persona_for_resume_test(user_id, persona_id).await;
        let key = RESUME_TEST_KEY_HEX;

        // 1. A run whose conversation is bound: the destination comes back,
        // and it is the BINDING, not a preference (none exists here).
        let run_id = seed_paused_api_chat_run(&pool, user_id, persona_id).await;
        create_binding(
            user_id,
            persona_id,
            key,
            "chat-bound",
            ChatKind::Api,
            "prov-bound",
            "model-bound",
        )
        .await
        .unwrap();
        crate::persistence::message_store::save_message(
            user_id,
            persona_id,
            key,
            "chat-bound",
            "assistant",
            "",
            Some(&run_id),
        )
        .await
        .unwrap();
        let run = rehydrate_api_chat(&pool, user_id, persona_id, &run_id)
            .await
            .expect("a bound run rehydrates");
        assert_eq!(
            run.chat_binding,
            Some(crate::api_chat::BoundDestination {
                provider_id: "prov-bound".to_owned(),
                model_id: "model-bound".to_owned(),
            })
        );
        assert!(
            run.chat_stream.is_none(),
            "a resumed run has no stream, as documented"
        );

        // 2. The reply message exists but its conversation has no binding.
        let run_id = seed_paused_api_chat_run(&pool, user_id, persona_id).await;
        crate::persistence::message_store::save_message(
            user_id,
            persona_id,
            key,
            "chat-unbound",
            "assistant",
            "",
            Some(&run_id),
        )
        .await
        .unwrap();
        let err = rehydrate_api_chat(&pool, user_id, persona_id, &run_id)
            .await
            .err()
            .expect("no binding means the run does not continue");
        assert!(
            err.to_string().contains("api_chat.binding_missing"),
            "{err}"
        );

        // 3. The conversation is bound, but as a QR chat: not an API destination.
        let run_id = seed_paused_api_chat_run(&pool, user_id, persona_id).await;
        create_binding(
            user_id,
            persona_id,
            key,
            "chat-qr-kind",
            ChatKind::Qr,
            "prov-bound",
            "model-bound",
        )
        .await
        .unwrap();
        crate::persistence::message_store::save_message(
            user_id,
            persona_id,
            key,
            "chat-qr-kind",
            "assistant",
            "",
            Some(&run_id),
        )
        .await
        .unwrap();
        let err = rehydrate_api_chat(&pool, user_id, persona_id, &run_id)
            .await
            .err()
            .expect("a QR-kind binding is not an API destination");
        assert!(
            err.to_string().contains("api_chat.binding_missing"),
            "{err}"
        );

        // 4. The run has no reply message at all.
        let run_id = seed_paused_api_chat_run(&pool, user_id, persona_id).await;
        let err = rehydrate_api_chat(&pool, user_id, persona_id, &run_id)
            .await
            .err()
            .expect("no reply message means no binding to find");
        assert!(
            err.to_string().contains("api_chat.binding_missing"),
            "{err}"
        );

        match saved_root {
            Some(v) => std::env::set_var("QR_DATA_ROOT", v),
            None => std::env::remove_var("QR_DATA_ROOT"),
        }
    }

    /// End-to-end (minus the model call) proof that a paused quick-ask run
    /// survives a rehydrate: real shared.db + real quick-ask.focus (the
    /// shipped artifact, one step: "draft") + a real checkpoint written via
    /// write_checkpoint() itself (production code, not hand-crafted JSON) +
    /// a hand-seeded focus_runs row standing in for AUTHORIZE (which never
    /// ran in this test, same as failure_ready_run()'s reasoning above).
    #[tokio::test]
    async fn rehydrate_focus_run_restores_a_paused_runs_checkpoint_and_user_input() {
        let _lock = crate::test_support::ENV_MUTEX.lock().await;
        let saved_root = std::env::var("QR_DATA_ROOT").ok();
        let tempdir = tempfile::tempdir().expect("failed to create tempdir");
        std::env::set_var("QR_DATA_ROOT", tempdir.path());

        let user_id = "resume-user";
        let persona_id = "resume-persona";
        let pool = setup_shared_and_persona_for_resume_test(user_id, persona_id).await;
        let focus_run_id = uuid::Uuid::new_v4().to_string();

        // Seed the focus_runs row first -- stands in for AUTHORIZE, which
        // never ran in this test (same reasoning as failure_ready_run()
        // above). Must come before the checkpoint write below:
        // focus_run_snapshots.focus_run_id REFERENCES focus_runs(id), enforced
        // (connect_options_encrypted sets foreign_keys(true)).
        {
            let mut conn = open_outputs_db(user_id, persona_id, RESUME_TEST_KEY_HEX)
                .await
                .expect("open_outputs_db must succeed");
            sqlx::query(
                "INSERT INTO focus_runs
                 (id, focus_id, status, is_fast_lane, is_quick_ask, topic_id, user_input, started_at)
                 VALUES (?, 'quick-ask', 'paused', 0, 1, NULL, ?, ?)",
            )
            .bind(&focus_run_id)
            .bind("Test user input for resume")
            .bind(now())
            .execute(&mut conn)
            .await
            .expect("seed focus_runs insert must succeed");
        }

        // Seed a real checkpoint via write_checkpoint() itself.
        {
            let scheduler = Arc::new(ConductorScheduler::new());
            let mut seed_run: FocusRun = FocusRun::new(
                user_id.to_owned(),
                persona_id.to_owned(),
                "quick-ask".to_owned(),
                pool.clone(),
                scheduler,
                "Test user input for resume".to_owned(),
                false,
                Some(RESUME_TEST_KEY_HEX.to_owned()),
                None,
                true,
                std::collections::HashSet::new(),
                None,
            );
            seed_run.focus_run_id = Some(focus_run_id.clone());
            let mut personal_track = PersonalTrack::new();
            personal_track.seal();
            seed_run.personal_track = Some(personal_track);
            let mut task_track = TaskTrack::new();
            task_track.add_step(TaskStep {
                action_id: "draft".to_owned(),
                output_var: Some("draft_output".to_owned()),
                content: "Original draft output".to_owned(),
                sensitivity_severity: 1,
                routing_tier_used: 1,
                personal_flagged: false,
                audit_unavailable: false,
            });
            seed_run.task_track = Some(task_track);
            seed_run.shared_state = Some(SharedStateTrack::new());
            seed_run
                .write_checkpoint("draft")
                .await
                .expect("write_checkpoint must succeed");
        }

        let scheduler = Arc::new(ConductorScheduler::new());
        let run: FocusRun = rehydrate_focus_run(
            user_id.to_owned(),
            persona_id.to_owned(),
            focus_run_id.clone(),
            pool.clone(),
            scheduler,
            Some(RESUME_TEST_KEY_HEX.to_owned()),
            std::collections::HashSet::new(),
            None,
        )
        .await
        .expect("rehydrate_focus_run must succeed");

        assert_eq!(run.focus_id, "quick-ask");
        assert!(
            run.chat_binding.is_none(),
            "only an api-chat run re-derives a binding"
        );
        assert_eq!(
            run.user_input, "Test user input for resume",
            "user_input must round-trip from focus_runs.user_input (outputs_010.sql)"
        );
        assert!(run.is_quick_ask);
        assert_eq!(
            run.current_step, 1,
            "status='paused' resumes AFTER the checkpointed step -- 'draft' is \
             quick-ask.focus's only step, at index 0, so 1 means execute() will \
             correctly find no remaining steps rather than re-running 'draft'"
        );
        assert_eq!(
            run.task_track.as_ref().unwrap().last_output(),
            Some("Original draft output"),
            "task_track must be rebuilt from the checkpoint's stored JSON, not left empty"
        );
        assert!(
            run.personal_track.as_ref().unwrap().is_sealed(),
            "rehydrate_focus_run must seal the freshly-fetched PersonalTrack, same as initialize()"
        );

        if let Some(v) = saved_root {
            std::env::set_var("QR_DATA_ROOT", v);
        } else {
            std::env::remove_var("QR_DATA_ROOT");
        }
    }

    /// items.id=587 regression test: demote_interrupted_runs()'s WHERE
    /// clause used to compare started_at (RFC3339, 'T'-separated, a
    /// "+00:00" offset -- see providers::utils::now()) against
    /// datetime('now', ...)'s own space-separated, offset-less output using
    /// a plain TEXT `<`. 'T' (0x54) sorts after a space (0x20), so
    /// started_at always read as lexicographically later than the cutoff,
    /// and the UPDATE matched zero rows, always, regardless of real elapsed
    /// time -- confirmed live (2026-10-03): a run left at status='running'
    /// by a killed process was still 'running' after 6+ real minutes and
    /// two logins. The one existing coverage for this function
    /// (auth.rs's login_demotes_a_stale_running_focus_run_to_paused) seeded
    /// started_at as '2020-01-01T00:00:00Z' -- six years stale, so the YEAR
    /// digits alone decided the lexicographic comparison and masked the
    /// 'T'-vs-space bug entirely. This test seeds a same-day, just-stale
    /// timestamp (now() - 10 minutes, the same RFC3339 shape real code
    /// produces) specifically to catch that class of masking, plus a
    /// just-started row that must survive -- guards against a fix that
    /// over-corrects into "always demote everything".
    #[tokio::test]
    async fn demote_interrupted_runs_demotes_a_same_day_stale_running_run() {
        let _lock = crate::test_support::ENV_MUTEX.lock().await;
        let saved_root = std::env::var("QR_DATA_ROOT").ok();
        let tempdir = tempfile::tempdir().expect("failed to create tempdir");
        std::env::set_var("QR_DATA_ROOT", tempdir.path());

        let user_id = "demote-test-user";
        let persona_id = "demote-test-persona";
        setup_shared_and_persona_for_resume_test(user_id, persona_id).await;

        let stale_run_id = uuid::Uuid::new_v4().to_string();
        let fresh_run_id = uuid::Uuid::new_v4().to_string();
        {
            let mut conn = open_outputs_db(user_id, persona_id, RESUME_TEST_KEY_HEX)
                .await
                .expect("open_outputs_db must succeed");
            let stale_started_at = (Utc::now() - Duration::minutes(10)).to_rfc3339();
            sqlx::query(
                "INSERT INTO focus_runs (id, focus_id, status, started_at)
                 VALUES (?, 'quick-ask', 'running', ?)",
            )
            .bind(&stale_run_id)
            .bind(&stale_started_at)
            .execute(&mut conn)
            .await
            .expect("seeding the stale focus_runs row must succeed");

            sqlx::query(
                "INSERT INTO focus_runs (id, focus_id, status, started_at)
                 VALUES (?, 'quick-ask', 'running', ?)",
            )
            .bind(&fresh_run_id)
            .bind(now())
            .execute(&mut conn)
            .await
            .expect("seeding the fresh focus_runs row must succeed");
        }

        let demoted = demote_interrupted_runs(user_id, persona_id, RESUME_TEST_KEY_HEX)
            .await
            .expect("demote_interrupted_runs must succeed");
        assert_eq!(
            demoted,
            Some(1),
            "exactly the stale run must be demoted, not the fresh one"
        );

        let mut conn = open_outputs_db(user_id, persona_id, RESUME_TEST_KEY_HEX)
            .await
            .expect("open_outputs_db must succeed for the post-demote read");
        let stale_status: String = sqlx::query_scalar("SELECT status FROM focus_runs WHERE id = ?")
            .bind(&stale_run_id)
            .fetch_one(&mut conn)
            .await
            .expect("stale row must still exist");
        assert_eq!(stale_status, "paused");

        let fresh_status: String = sqlx::query_scalar("SELECT status FROM focus_runs WHERE id = ?")
            .bind(&fresh_run_id)
            .fetch_one(&mut conn)
            .await
            .expect("fresh row must still exist");
        assert_eq!(
            fresh_status, "running",
            "a just-started run must not be demoted"
        );

        if let Some(v) = saved_root {
            std::env::set_var("QR_DATA_ROOT", v);
        } else {
            std::env::remove_var("QR_DATA_ROOT");
        }
    }

    /// items.id=794: a stale `running` row is left alone while the user has a
    /// reply or Focus run alive (a lease in the writer gate), and demoted once
    /// there is none. No lease at all is the fresh-process-after-a-crash case.
    /// Own user ids: the writer gate is a process-wide static.
    #[tokio::test]
    async fn demote_interrupted_runs_defers_while_a_reply_or_focus_run_is_alive() {
        use crate::auth::writer_gate::{acquire, Kind};

        with_temp_data_root(async {
            let user_id = "demote-lease-user";
            let other_user_id = "demote-lease-other-user";
            let persona_id = "demote-lease-persona";
            setup_shared_and_persona_for_resume_test(user_id, persona_id).await;

            let run_id = uuid::Uuid::new_v4().to_string();
            {
                let mut conn = open_outputs_db(user_id, persona_id, RESUME_TEST_KEY_HEX)
                    .await
                    .expect("open_outputs_db must succeed");
                sqlx::query(
                    "INSERT INTO focus_runs (id, focus_id, status, started_at)
                     VALUES (?, 'quick-ask', 'running', ?)",
                )
                .bind(&run_id)
                .bind((Utc::now() - Duration::minutes(10)).to_rfc3339())
                .execute(&mut conn)
                .await
                .expect("seeding the stale focus_runs row must succeed");
            }
            let status = || async {
                let mut conn = open_outputs_db(user_id, persona_id, RESUME_TEST_KEY_HEX)
                    .await
                    .expect("open_outputs_db must succeed for the read");
                sqlx::query_scalar::<_, String>("SELECT status FROM focus_runs WHERE id = ?")
                    .bind(&run_id)
                    .fetch_one(&mut conn)
                    .await
                    .expect("the row must still exist")
            };
            let demote = || async {
                demote_interrupted_runs(user_id, persona_id, RESUME_TEST_KEY_HEX)
                    .await
                    .expect("demote_interrupted_runs must succeed")
            };

            // A live reply, then a live Focus run: deferred, row untouched.
            let reply = acquire(user_id, Kind::Reply).unwrap();
            assert_eq!(demote().await, None);
            assert_eq!(status().await, "running");
            drop(reply);
            let focus_run = acquire(user_id, Kind::FocusRun).unwrap();
            assert_eq!(demote().await, None);
            assert_eq!(status().await, "running");
            drop(focus_run);

            // Another user's visible work does not block this user.
            let others = acquire(other_user_id, Kind::Reply).unwrap();
            // Short work of this user's own (the login's Command lease, the
            // recovery's Sweep lease, an ingest) is not visible work.
            let short = [
                acquire(user_id, Kind::Command).unwrap(),
                acquire(user_id, Kind::Sweep).unwrap(),
                acquire(user_id, Kind::Ingest).unwrap(),
            ];
            assert_eq!(demote().await, Some(1));
            assert_eq!(status().await, "paused");
            drop((others, short));

            // The same row, running again: with the lease gone (and in a fresh
            // process there never was one), the next call demotes it.
            {
                let mut conn = open_outputs_db(user_id, persona_id, RESUME_TEST_KEY_HEX)
                    .await
                    .expect("open_outputs_db must succeed");
                sqlx::query("UPDATE focus_runs SET status = 'running' WHERE id = ?")
                    .bind(&run_id)
                    .execute(&mut conn)
                    .await
                    .unwrap();
            }
            let reply = acquire(user_id, Kind::Reply).unwrap();
            assert_eq!(demote().await, None);
            assert_eq!(status().await, "running");
            drop(reply);
            assert_eq!(demote().await, Some(1));
            assert_eq!(status().await, "paused");
        })
        .await;
    }

    /// resume_execution() itself: promotes to 'running', then drives a run
    /// with zero remaining steps straight through Phase 5 OUTPUT to
    /// 'complete' -- without calling initialize() (which would be the bug:
    /// initialize() unconditionally resets task_track/shared_state and would
    /// be silently "fine" here only because this fixture's task_track is
    /// already empty; the real guarantee is structural -- execute_and_finish()
    /// contains no call to initialize() at all, see its own doc comment).
    /// task_track is deliberately empty (not seeded with a completed step)
    /// so the Phase 5 extraction pass's step_outputs.is_empty() short-circuit
    /// (extract.rs) is guaranteed to fire -- this test must never depend on
    /// a reachable Ollama instance.
    #[tokio::test]
    async fn resume_execution_completes_a_run_with_no_steps_remaining() {
        with_temp_data_root(async {
            let scheduler = Arc::new(ConductorScheduler::new());
            let mut run: FocusRun<crate::conductor::privacy::logger::TestLogger> = FocusRun::new(
                "u".to_owned(),
                "p".to_owned(),
                "f".to_owned(),
                dummy_pool(),
                scheduler,
                "".to_owned(),
                false,
                Some(CRISIS_TEST_KEY_HEX.to_owned()),
                None,
                false,
                std::collections::HashSet::new(),
                None,
            );
            run.focus_run_id = Some(uuid::Uuid::new_v4().to_string());
            run.focus_def = Some(parse_focus_definition(minimal_raw()).unwrap());
            let mut personal_track = PersonalTrack::new();
            personal_track.seal();
            run.personal_track = Some(personal_track);
            run.task_track = Some(TaskTrack::new());
            run.shared_state = Some(SharedStateTrack::new());
            run.failure_handler = Some(FailureHandler::new(ExternalAccess::LocalOnly));
            run.privacy_gateway = Some(PrivacyGateway::new(
                crate::conductor::privacy::logger::TestLogger::for_run("u", "p", ""),
            ));
            run.current_step = 0;

            let focus_run_id = run.focus_run_id.clone().unwrap();
            let result = run
                .resume_execution()
                .await
                .expect("resume_execution must succeed");

            // output()'s own RunResult.status is always "awaiting_feedback"
            // (pre-existing behavior, unchanged by this item -- see
            // execute_and_finish()'s final `Ok(output_result)`, which returns
            // output()'s result as-is even though cleanup("complete") below
            // it has already overwritten focus_runs.status to 'complete').
            // The real, current status lives in the DB, checked below.
            assert_eq!(result.status, "awaiting_feedback");

            // Confirm resume_execution() actually persisted 'running' then
            // 'complete' against the real tempdir outputs.db, not just
            // returned an in-memory RunResult.
            let mut conn = open_outputs_db("u", "p", CRISIS_TEST_KEY_HEX)
                .await
                .expect("open_outputs_db must succeed");
            let row = sqlx::query("SELECT status FROM focus_runs WHERE id = ?")
                .bind(&focus_run_id)
                .fetch_one(&mut conn)
                .await
                .expect("focus_runs row must exist after resume_execution");
            let status: String = row.try_get("status").unwrap();
            assert_eq!(status, "complete");
        })
        .await;
    }

    /// items.id=778: a run whose lease was told to stop ends `cancelled` at the
    /// next step boundary without running another step, and writes
    /// `cancelled` to focus_runs. No step executes, so this needs no model.
    #[tokio::test]
    async fn a_run_stopped_for_a_key_rotation_ends_cancelled_without_running_a_step() {
        with_temp_data_root(async {
            let scheduler = Arc::new(ConductorScheduler::new());
            let mut run: FocusRun<crate::conductor::privacy::logger::TestLogger> = FocusRun::new(
                "u-stop".to_owned(),
                "p".to_owned(),
                "f".to_owned(),
                dummy_pool(),
                scheduler,
                "".to_owned(),
                false,
                Some(CRISIS_TEST_KEY_HEX.to_owned()),
                None,
                false,
                std::collections::HashSet::new(),
                None,
            );
            run.focus_run_id = Some(uuid::Uuid::new_v4().to_string());
            let mut def = parse_focus_definition(minimal_raw()).unwrap();
            def.steps.push(crate::conductor::tokens::ActionDefinition {
                action_id: "s1".to_owned(),
                display_name: "Step 1".to_owned(),
                guide_id: "g".to_owned(),
                task_type: "general".to_owned(),
                routing_tier: ExternalAccess::LocalOnly,
                step_type: Default::default(),
                output_var: None,
                prompt_template: "never rendered".to_owned(),
                field_requirements: std::collections::HashMap::new(),
                options_override: std::collections::HashMap::new(),
                external_access_override: None,
                requires_user_handoff: false,
                revisitable: true,
                block_stable_id: None,
                block_customization: None,
                schedule_trigger: None,
                guard: None,
                declared_inputs: Vec::new(),
                store_op: None,
                and_guards: Vec::new(),
            });
            run.focus_def = Some(def);
            let mut personal_track = PersonalTrack::new();
            personal_track.seal();
            run.personal_track = Some(personal_track);
            run.task_track = Some(TaskTrack::new());
            run.shared_state = Some(SharedStateTrack::new());
            run.failure_handler = Some(FailureHandler::new(ExternalAccess::LocalOnly));
            run.privacy_gateway = Some(PrivacyGateway::new(
                crate::conductor::privacy::logger::TestLogger::for_run("u-stop", "p", ""),
            ));

            // Not stopped: no stop is requested without a lease.
            assert!(!run.stop_requested());
            let lease = crate::auth::writer_gate::acquire(
                "u-stop",
                crate::auth::writer_gate::Kind::FocusRun,
            )
            .unwrap();
            let stop = lease.stop_token().clone();
            run.attach_lease(lease).unwrap();
            assert!(!run.stop_requested());
            stop.cancel();
            assert!(run.stop_requested());

            let early = run
                .execute()
                .await
                .expect("execute must succeed")
                .expect("a stopped run returns early");
            assert_eq!(early.status, "cancelled");
            assert!(early.failure.is_none());
            assert!(run.task_track.as_ref().unwrap().steps().is_empty());
        })
        .await;
    }

    /// A run with one local step and the minimal tracks `execute()` asserts on,
    /// as in the test above, for the items.id=816 cancel tests. No step can run
    /// without a model, so these only exercise the stop checks.
    fn one_step_run(user: &str) -> FocusRun<crate::conductor::privacy::logger::TestLogger> {
        let scheduler = Arc::new(ConductorScheduler::new());
        let mut run: FocusRun<crate::conductor::privacy::logger::TestLogger> = FocusRun::new(
            user.to_owned(),
            "p".to_owned(),
            "f".to_owned(),
            dummy_pool(),
            scheduler,
            "".to_owned(),
            false,
            Some(CRISIS_TEST_KEY_HEX.to_owned()),
            None,
            false,
            std::collections::HashSet::new(),
            None,
        );
        run.focus_run_id = Some(uuid::Uuid::new_v4().to_string());
        let mut def = parse_focus_definition(minimal_raw()).unwrap();
        def.steps.push(crate::conductor::tokens::ActionDefinition {
            action_id: "s1".to_owned(),
            display_name: "Step 1".to_owned(),
            guide_id: "g".to_owned(),
            task_type: "general".to_owned(),
            routing_tier: ExternalAccess::LocalOnly,
            step_type: Default::default(),
            output_var: None,
            prompt_template: "never rendered".to_owned(),
            field_requirements: std::collections::HashMap::new(),
            options_override: std::collections::HashMap::new(),
            external_access_override: None,
            requires_user_handoff: false,
            revisitable: true,
            block_stable_id: None,
            block_customization: None,
            schedule_trigger: None,
            guard: None,
            declared_inputs: Vec::new(),
            store_op: None,
            and_guards: Vec::new(),
        });
        run.focus_def = Some(def);
        let mut personal_track = PersonalTrack::new();
        personal_track.seal();
        run.personal_track = Some(personal_track);
        run.task_track = Some(TaskTrack::new());
        run.shared_state = Some(SharedStateTrack::new());
        run.failure_handler = Some(FailureHandler::new(ExternalAccess::LocalOnly));
        run.privacy_gateway = Some(PrivacyGateway::new(
            crate::conductor::privacy::logger::TestLogger::for_run(user, "p", ""),
        ));
        run
    }

    /// items.id=816: a user's cancel, by run id, stops a run the way a rotation's
    /// stop does: it ends `cancelled` with no failure and runs no step.
    #[tokio::test]
    async fn a_run_cancelled_by_its_id_ends_cancelled_without_running_a_step() {
        with_temp_data_root(async {
            let mut run = one_step_run("u-cancel-run");
            let run_id = run.focus_run_id.clone().unwrap();
            run.attach_lease(
                crate::auth::writer_gate::acquire(
                    "u-cancel-run",
                    crate::auth::writer_gate::Kind::FocusRun,
                )
                .unwrap(),
            )
            .unwrap();
            assert!(!run.stop_requested());

            assert_eq!(
                crate::auth::writer_gate::request_stop_for_run("u-cancel-run", &run_id),
                1
            );
            assert!(run.stop_requested());
            let early = run
                .execute()
                .await
                .expect("execute must succeed")
                .expect("a cancelled run returns early");
            assert_eq!(early.status, "cancelled");
            assert!(early.failure.is_none());
            assert!(run.task_track.as_ref().unwrap().steps().is_empty());
        })
        .await;
    }

    /// Another user's cancel, or a cancel naming another run, leaves the run
    /// going.
    #[tokio::test]
    async fn a_cancel_for_another_user_or_run_does_not_stop_the_run() {
        with_temp_data_root(async {
            let mut run = one_step_run("u-cancel-other");
            let run_id = run.focus_run_id.clone().unwrap();
            run.attach_lease(
                crate::auth::writer_gate::acquire(
                    "u-cancel-other",
                    crate::auth::writer_gate::Kind::Reply,
                )
                .unwrap(),
            )
            .unwrap();
            assert_eq!(
                crate::auth::writer_gate::request_stop_for_run("someone-else", &run_id),
                0
            );
            assert_eq!(
                crate::auth::writer_gate::request_stop_for_run("u-cancel-other", "another-run"),
                0
            );
            assert!(!run.stop_requested());
        })
        .await;
    }

    /// The lease is released, and the cancel finds nothing, once the run is
    /// dropped (a crashed or finished run leaves no entry).
    #[tokio::test]
    async fn a_dropped_run_leaves_nothing_to_cancel() {
        with_temp_data_root(async {
            let mut run = one_step_run("u-cancel-drop");
            let run_id = run.focus_run_id.clone().unwrap();
            run.attach_lease(
                crate::auth::writer_gate::acquire(
                    "u-cancel-drop",
                    crate::auth::writer_gate::Kind::FocusRun,
                )
                .unwrap(),
            )
            .unwrap();
            drop(run);
            assert_eq!(
                crate::auth::writer_gate::request_stop_for_run("u-cancel-drop", &run_id),
                0
            );
            assert!(crate::auth::writer_gate::is_idle("u-cancel-drop"));
        })
        .await;
    }

    /// A lease is never attached without a run id to bind it to: it fails, the
    /// run holds no lease, and the lease is released.
    #[tokio::test]
    async fn attach_lease_without_a_run_id_fails_and_attaches_nothing() {
        with_temp_data_root(async {
            let mut run = one_step_run("u-cancel-noid");
            run.focus_run_id = None;
            let lease = crate::auth::writer_gate::acquire(
                "u-cancel-noid",
                crate::auth::writer_gate::Kind::FocusRun,
            )
            .unwrap();
            let err = run.attach_lease(lease).unwrap_err();
            assert!(matches!(err, LifecycleError::ValidationFailed(_)));
            assert!(run.lease.is_none());
            assert!(crate::auth::writer_gate::is_idle("u-cancel-noid"));
        })
        .await;
    }

    /// An API Chat Stop during a retry wait reaches the lifecycle as a USER_STOP
    /// failure with the lease token untripped: that ends the run; an ordinary
    /// failure does not (items.id=816).
    #[tokio::test]
    async fn a_user_stop_failure_ends_the_run_and_an_ordinary_failure_does_not() {
        with_temp_data_root(async {
            let run = one_step_run("u-cancel-failure");
            let user_stop = FailureResult {
                action: FailureAction::Stop,
                failure_mode: Some(crate::conductor::executor::USER_STOP_MODE.to_owned()),
                plain_language: "This was stopped.".to_owned(),
                is_recoverable: false,
                severity: FailureSeverity::Stop,
                action_id: Some("s1".to_owned()),
                focus_id: Some("f".to_owned()),
                metadata: None,
            };
            assert!(run.failure_ends_run(&user_stop));
            let ordinary = FailureResult {
                action: FailureAction::AwaitUser,
                failure_mode: Some("F10".to_owned()),
                is_recoverable: true,
                ..user_stop.clone()
            };
            assert!(!run.failure_ends_run(&ordinary));
            // A Stop failure of another mode (a real failure) does not end it either.
            let failed = FailureResult {
                failure_mode: Some("F4".to_owned()),
                ..user_stop
            };
            assert!(!run.failure_ends_run(&failed));
        })
        .await;
    }

    // -------------------------------------------------------------------------
    // reenter_step() (items.id=574 follow-up, cb-07)
    // -------------------------------------------------------------------------

    /// End-to-end: a real writing-assistant run (3 real steps --
    /// voice_analysis/generate, draft_writing/generate, voice_transform/
    /// voice_transform -- all revisitable=true) that already completed all
    /// three steps, reentered at the middle step. Proves the plan is
    /// applied for real: task_track rebuilt to only the preserved step,
    /// current_step rewound, focus_runs.status flipped to 'awaiting_user'
    /// (required for a future rehydrate to resume AT, not after, the
    /// checkpointed step), a checkpoint written, and focus_run_steps rows
    /// written for the first time ever against this run (this table has no
    /// other writer anywhere in the codebase yet).
    #[tokio::test]
    async fn reenter_step_middle_step_rewinds_track_and_persists_state() {
        let _lock = crate::test_support::ENV_MUTEX.lock().await;
        let saved_root = std::env::var("QR_DATA_ROOT").ok();
        let tempdir = tempfile::tempdir().expect("failed to create tempdir");
        std::env::set_var("QR_DATA_ROOT", tempdir.path());

        let user_id = "reentry-user";
        let persona_id = "reentry-persona";
        let pool = setup_shared_and_persona_for_resume_test(user_id, persona_id).await;
        let focus_run_id = uuid::Uuid::new_v4().to_string();

        {
            let mut conn = open_outputs_db(user_id, persona_id, RESUME_TEST_KEY_HEX)
                .await
                .expect("open_outputs_db must succeed");
            sqlx::query(
                "INSERT INTO focus_runs
                 (id, focus_id, status, is_fast_lane, is_quick_ask, topic_id, user_input, started_at)
                 VALUES (?, 'writing-assistant', 'awaiting_feedback', 0, 0, NULL, ?, ?)",
            )
            .bind(&focus_run_id)
            .bind("Test user input for reentry")
            .bind(now())
            .execute(&mut conn)
            .await
            .expect("seed focus_runs insert must succeed");
        }

        // Seed a real checkpoint (all 3 steps complete) via write_checkpoint()
        // itself, same pattern as the rehydrate test above.
        {
            let scheduler = Arc::new(ConductorScheduler::new());
            let mut seed_run: FocusRun = FocusRun::new(
                user_id.to_owned(),
                persona_id.to_owned(),
                "writing-assistant".to_owned(),
                pool.clone(),
                scheduler,
                "Test user input for reentry".to_owned(),
                false,
                Some(RESUME_TEST_KEY_HEX.to_owned()),
                None,
                false,
                std::collections::HashSet::new(),
                None,
            );
            seed_run.focus_run_id = Some(focus_run_id.clone());
            let mut personal_track = PersonalTrack::new();
            personal_track.seal();
            seed_run.personal_track = Some(personal_track);
            let mut task_track = TaskTrack::new();
            task_track.add_step(TaskStep {
                action_id: "voice_analysis".to_owned(),
                output_var: Some("voice_analysis_output".to_owned()),
                content: "Analysis".to_owned(),
                sensitivity_severity: 1,
                routing_tier_used: 1,
                personal_flagged: false,
                audit_unavailable: false,
            });
            task_track.add_step(TaskStep {
                action_id: "draft_writing".to_owned(),
                output_var: Some("draft_output".to_owned()),
                content: "Original draft".to_owned(),
                sensitivity_severity: 1,
                routing_tier_used: 2,
                personal_flagged: false,
                audit_unavailable: false,
            });
            task_track.add_step(TaskStep {
                action_id: "quality_check".to_owned(),
                output_var: Some("draft_quality_verdict".to_owned()),
                content: "Quality check (advisory): passed.".to_owned(),
                sensitivity_severity: 1,
                routing_tier_used: 1,
                personal_flagged: false,
                audit_unavailable: false,
            });
            task_track.add_step(TaskStep {
                action_id: "voice_transform".to_owned(),
                output_var: Some("final_output".to_owned()),
                content: "Original final".to_owned(),
                sensitivity_severity: 1,
                routing_tier_used: 1,
                personal_flagged: false,
                audit_unavailable: false,
            });
            seed_run.task_track = Some(task_track);
            seed_run.shared_state = Some(SharedStateTrack::new());
            seed_run
                .write_checkpoint("voice_transform")
                .await
                .expect("write_checkpoint must succeed");
        }

        let scheduler = Arc::new(ConductorScheduler::new());
        let mut run: FocusRun = rehydrate_focus_run(
            user_id.to_owned(),
            persona_id.to_owned(),
            focus_run_id.clone(),
            pool.clone(),
            scheduler,
            Some(RESUME_TEST_KEY_HEX.to_owned()),
            std::collections::HashSet::new(),
            None,
        )
        .await
        .expect("rehydrate_focus_run must succeed");

        let focus_step_order: Vec<String> = run
            .focus_def
            .as_ref()
            .unwrap()
            .steps
            .iter()
            .map(|s| s.action_id.clone())
            .collect();
        assert_eq!(
            focus_step_order,
            vec![
                "voice_analysis".to_owned(),
                "draft_writing".to_owned(),
                "quality_check".to_owned(),
                "voice_transform".to_owned()
            ],
            "writing-assistant now loads from the DB path with its cb-06 step (items.id=129)"
        );

        let plan = run
            .reenter_step(&focus_step_order, "draft_writing")
            .await
            .expect("reenter_step must succeed");

        assert_eq!(plan.resume_from_index, 1);
        assert_eq!(plan.preserved_action_ids, vec!["voice_analysis".to_owned()]);
        assert_eq!(
            plan.discarded_action_ids,
            vec![
                "draft_writing".to_owned(),
                "quality_check".to_owned(),
                "voice_transform".to_owned()
            ]
        );
        // The block's verdict variable goes stale with the steps it described.
        assert!(
            plan.stale_output_vars
                .contains(&"draft_quality_verdict".to_owned()),
            "{:?}",
            plan.stale_output_vars
        );

        assert_eq!(run.current_step, 1);
        assert_eq!(
            run.task_track.as_ref().unwrap().steps().len(),
            1,
            "rebuilt task_track must keep only the preserved (pre-target) step"
        );
        assert_eq!(
            run.task_track.as_ref().unwrap().last_output(),
            Some("Analysis")
        );

        let mut conn = open_outputs_db(user_id, persona_id, RESUME_TEST_KEY_HEX)
            .await
            .expect("open_outputs_db must succeed");

        let status: String = sqlx::query_scalar("SELECT status FROM focus_runs WHERE id = ?")
            .bind(&focus_run_id)
            .fetch_one(&mut conn)
            .await
            .unwrap();
        assert_eq!(
            status, "awaiting_user",
            "must be set so a future rehydrate resumes AT the checkpointed \
             step (target_action_id), not one index past it"
        );

        // First-ever write to focus_run_steps for this run: only
        // draft_writing gets a fresh row (it's the reentry target); the
        // other discarded step (voice_transform) never had a live row to
        // begin with, so its UPDATE was a no-op -- correct, not a bug.
        let rows: Vec<(String, i64, String)> = sqlx::query_as(
            "SELECT action_id, attempt, status FROM focus_run_steps
             WHERE focus_run_id = ? ORDER BY action_id",
        )
        .bind(&focus_run_id)
        .fetch_all(&mut conn)
        .await
        .unwrap();
        assert_eq!(
            rows,
            vec![("draft_writing".to_owned(), 1, "pending".to_owned())]
        );
        drop(conn);

        // Simulate forward execution having continued past the rewind and
        // completed draft_writing and voice_transform again -- out of scope
        // for reenter_step() itself (that's execute()'s own forward loop,
        // exercised separately; ticket point 3 confirms it needs no change).
        run.task_track.as_mut().unwrap().add_step(TaskStep {
            action_id: "draft_writing".to_owned(),
            output_var: Some("draft_output".to_owned()),
            content: "Second draft".to_owned(),
            sensitivity_severity: 1,
            routing_tier_used: 2,
            personal_flagged: false,
            audit_unavailable: false,
        });
        run.task_track.as_mut().unwrap().add_step(TaskStep {
            action_id: "quality_check".to_owned(),
            output_var: Some("draft_quality_verdict".to_owned()),
            content: "Quality check (advisory): passed.".to_owned(),
            sensitivity_severity: 1,
            routing_tier_used: 1,
            personal_flagged: false,
            audit_unavailable: false,
        });
        run.task_track.as_mut().unwrap().add_step(TaskStep {
            action_id: "voice_transform".to_owned(),
            output_var: Some("final_output".to_owned()),
            content: "Second final".to_owned(),
            sensitivity_severity: 1,
            routing_tier_used: 1,
            personal_flagged: false,
            audit_unavailable: false,
        });

        // Reenter draft_writing again -- the HARD INVARIANT under test:
        // attempt must be MAX(attempt)+1 for (focus_run_id, action_id) =
        // (this run, draft_writing), i.e. 2 (the first call's row is still
        // attempt=1/'pending'), never reset to 1 and never independently
        // derived from anything else.
        let plan2 = run
            .reenter_step(&focus_step_order, "draft_writing")
            .await
            .expect("second reenter_step of the same step must succeed");
        assert_eq!(plan2.resume_from_index, 1);
        assert_eq!(
            plan2.preserved_action_ids,
            vec!["voice_analysis".to_owned()]
        );

        let mut conn = open_outputs_db(user_id, persona_id, RESUME_TEST_KEY_HEX)
            .await
            .expect("open_outputs_db must succeed");
        let rows: Vec<(String, i64, String)> = sqlx::query_as(
            "SELECT action_id, attempt, status FROM focus_run_steps
             WHERE focus_run_id = ? ORDER BY action_id, attempt",
        )
        .bind(&focus_run_id)
        .fetch_all(&mut conn)
        .await
        .unwrap();
        assert_eq!(
            rows,
            vec![
                ("draft_writing".to_owned(), 1, "discarded".to_owned()),
                ("draft_writing".to_owned(), 2, "pending".to_owned()),
            ]
        );
        drop(conn);

        // task_track was rebuilt back down to just [voice_analysis] again --
        // reentering it now (the track's only/most recent step) must hit
        // NothingToDiscard, same as plan_reentry's own unit test coverage.
        let plan3 = run.reenter_step(&focus_step_order, "voice_analysis").await;
        assert!(
            matches!(
                plan3,
                Err(LifecycleError::Reentry(ReentryError::NothingToDiscard(_)))
            ),
            "voice_analysis is now the track's only (most recent) step -- \
             nothing after it to discard"
        );

        if let Some(v) = saved_root {
            std::env::set_var("QR_DATA_ROOT", v);
        } else {
            std::env::remove_var("QR_DATA_ROOT");
        }
    }

    // -- items.id=496: DB-composition Focus + schedule_trigger gate --------
    //
    // End-to-end (minus the model call for the "already active" half, which
    // needs a real Ollama instance -- see the #[ignore]d test below)
    // verification that a Focus authored entirely as shared.db rows
    // (shared_021.sql: focuses/focus_block_compositions/
    // focus_block_customization), not a .focus YAML file, loads correctly
    // via load_focus_definition_from_db() and that execute()'s
    // schedule_trigger gate parks the run at 'awaiting_schedule' with a
    // real focus_run_steps.scheduled_for -- exactly the mechanism
    // conductor::scheduled_sweep queries.

    /// Seeds one composition-authored Focus ("sched-focus", one step,
    /// block_stable_id NULL) with a schedule_trigger anchored on a real
    /// Topic's extra_metadata field, plus the focus_settings row AUTHORIZE-
    /// equivalent code (get_focus_tier_ceiling(), called from
    /// resolve_tier_config()) hard-requires. Returns (pool, topic_id).
    async fn seed_db_composed_focus_with_schedule_trigger(
        user_id: &str,
        persona_id: &str,
        key_hex_str: &str,
    ) -> (sqlx::SqlitePool, String) {
        let pool = setup_shared_and_persona_for_resume_test(user_id, persona_id).await;

        crate::persistence::focus_settings_store::create_focus_settings(
            &pool,
            persona_id,
            "sched-focus",
            "isolated",
            "persona_hidden",
            1,
            ExternalAccess::LocalOnly,
            "protected",
            None,
        )
        .await
        .expect("create_focus_settings must succeed in test setup");

        let mut conn = pool.acquire().await.unwrap();
        sqlx::query(
            "INSERT INTO focuses
                (focus_id, display_name, description, version, max_routing_tier,
                 output_type, suggest_in_focuses, multi_source_validation,
                 generic_title_template, status, created_at, updated_at)
             VALUES ('sched-focus', 'Scheduled Test Focus', '', '1.0', 'local_only',
                     'general', '[]', 0, 'Hidden item', 'shipped',
                     datetime('now'), datetime('now'))",
        )
        .execute(&mut *conn)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO focus_block_compositions
                (id, focus_id, action_id, block_stable_id, sequence_index,
                 schedule_trigger_anchor_field, schedule_trigger_offset,
                 created_at, updated_at)
             VALUES ('comp-sched-1', 'sched-focus', 'step_a', NULL, 0,
                     'ship_date', '+0', datetime('now'), datetime('now'))",
        )
        .execute(&mut *conn)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO focus_block_customization
                (id, composition_id, persona_id, customization, created_at, updated_at)
             VALUES ('cust-sched-1', 'comp-sched-1', NULL, ?, datetime('now'), datetime('now'))",
        )
        .bind(
            serde_json::json!({
                "task_type": "quick_response",
                "routing_tier": 1,
                "prompt_template": "Say hello back to {user_input}",
                "output_var": "result"
            })
            .to_string(),
        )
        .execute(&mut *conn)
        .await
        .unwrap();
        drop(conn);

        let topic = crate::persistence::topic_store::create_topic(
            user_id,
            persona_id,
            key_hex_str,
            "sched-focus",
            None,
            None,
        )
        .await
        .expect("create_topic must succeed in test setup");

        (pool, topic.id)
    }

    /// Direct raw-SQL update of a Topic's extra_metadata -- no
    /// topic_store setter exists for this single field, and adding one
    /// only for this test's sake would be scope creep past items.id=496.
    async fn set_topic_anchor(
        user_id: &str,
        persona_id: &str,
        key_hex_str: &str,
        topic_id: &str,
        anchor_field: &str,
        anchor: DateTime<Utc>,
    ) {
        let mut conn = open_outputs_db(user_id, persona_id, key_hex_str)
            .await
            .unwrap();
        sqlx::query("UPDATE topics SET extra_metadata = ? WHERE id = ?")
            .bind(serde_json::json!({ anchor_field: anchor.to_rfc3339() }).to_string())
            .bind(topic_id)
            .execute(&mut conn)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn db_composed_focus_schedule_trigger_not_yet_active_parks_awaiting_schedule() {
        let _lock = crate::test_support::ENV_MUTEX.lock().await;
        let saved_root = std::env::var("QR_DATA_ROOT").ok();
        let tempdir = tempfile::tempdir().expect("failed to create tempdir");
        std::env::set_var("QR_DATA_ROOT", tempdir.path());

        let user_id = "sched-user";
        let persona_id = "sched-persona";
        let key_hex_str = RESUME_TEST_KEY_HEX;
        let (pool, topic_id) =
            seed_db_composed_focus_with_schedule_trigger(user_id, persona_id, key_hex_str).await;

        // Anchor 2 hours in the future, offset '+0' -- trigger.is_active()
        // is false, so the very first execute() iteration must park, never
        // reaching execute_step() (no model call, matching this test's own
        // "no ignore needed" scope).
        set_topic_anchor(
            user_id,
            persona_id,
            key_hex_str,
            &topic_id,
            "ship_date",
            Utc::now() + Duration::hours(2),
        )
        .await;

        let focus_run_id = uuid::Uuid::new_v4().to_string();
        {
            let mut conn = open_outputs_db(user_id, persona_id, key_hex_str)
                .await
                .unwrap();
            sqlx::query(
                "INSERT INTO focus_runs
                 (id, focus_id, status, is_fast_lane, is_quick_ask, topic_id, user_input, started_at)
                 VALUES (?, 'sched-focus', 'initializing', 0, 0, ?, 'hello', ?)",
            )
            .bind(&focus_run_id)
            .bind(&topic_id)
            .bind(now())
            .execute(&mut conn)
            .await
            .unwrap();
        }

        let scheduler = Arc::new(ConductorScheduler::new());
        let mut run: FocusRun = rehydrate_focus_run(
            user_id.to_owned(),
            persona_id.to_owned(),
            focus_run_id.clone(),
            pool.clone(),
            scheduler,
            Some(key_hex_str.to_owned()),
            std::collections::HashSet::new(),
            None,
        )
        .await
        .expect("rehydrate_focus_run must load the DB-composed Focus and succeed");

        // Proves load_focus_definition_from_db() actually ran (not a
        // fallback to a .focus YAML file, which has no file named
        // 'sched-focus') and that schedule_trigger parsed correctly.
        let step = &run.focus_def.as_ref().unwrap().steps[0];
        assert_eq!(step.action_id, "step_a");
        assert_eq!(step.block_stable_id, None);
        assert_eq!(
            step.schedule_trigger.as_ref().unwrap().anchor_field,
            "ship_date"
        );

        let result = run
            .resume_execution()
            .await
            .expect("resume_execution must succeed even when parking");
        assert_eq!(result.status, "awaiting_schedule");

        // The ledger row (items.id=574 piece (c)) exists with scheduled_for
        // set -- exactly what conductor::scheduled_sweep's query needs.
        let mut conn = open_outputs_db(user_id, persona_id, key_hex_str)
            .await
            .unwrap();
        let scheduled_for: Option<String> = sqlx::query_scalar(
            "SELECT scheduled_for FROM focus_run_steps
             WHERE focus_run_id = ? AND action_id = 'step_a' AND status != 'discarded'",
        )
        .bind(&focus_run_id)
        .fetch_one(&mut conn)
        .await
        .unwrap();
        assert!(
            scheduled_for.is_some(),
            "the gate must set scheduled_for before parking"
        );
        drop(conn);

        // The sweep's own read side must NOT surface this run yet -- the
        // anchor is still 2 hours out.
        let due = crate::persistence::output_store::list_due_scheduled_focus_runs(
            user_id,
            persona_id,
            key_hex_str,
            &now(),
        )
        .await
        .unwrap();
        assert!(
            !due.contains(&focus_run_id),
            "must not be due while the anchor is still in the future"
        );

        // Simulate wall-clock time passing: the gate wrote scheduled_for as
        // a snapshot (anchor+offset) at park time -- the sweep's query
        // reads that persisted column, not a live re-evaluation of the
        // Topic's anchor, so simulating "later" means advancing
        // scheduled_for directly, exactly what a real clock tick would do
        // to the comparison `scheduled_for <= now()` without anyone having
        // touched the Topic at all. A fresh rehydrate must then resume AT
        // the same step index (0) -- not skip past it -- confirming the
        // resume-index fix (rehydrate_focus_run's status=='awaiting_schedule'
        // arm) actually holds against a real checkpoint written by the real
        // gate above, not a hand-constructed one.
        {
            let mut conn = open_outputs_db(user_id, persona_id, key_hex_str)
                .await
                .unwrap();
            sqlx::query(
                "UPDATE focus_run_steps SET scheduled_for = ?
                 WHERE focus_run_id = ? AND action_id = 'step_a'",
            )
            .bind((Utc::now() - Duration::hours(2)).to_rfc3339())
            .bind(&focus_run_id)
            .execute(&mut conn)
            .await
            .unwrap();
        }

        let due_after = crate::persistence::output_store::list_due_scheduled_focus_runs(
            user_id,
            persona_id,
            key_hex_str,
            &now(),
        )
        .await
        .unwrap();
        assert!(
            due_after.contains(&focus_run_id),
            "must be due once the anchor has passed"
        );

        let scheduler2 = Arc::new(ConductorScheduler::new());
        let run2: FocusRun = rehydrate_focus_run(
            user_id.to_owned(),
            persona_id.to_owned(),
            focus_run_id.clone(),
            pool.clone(),
            scheduler2,
            Some(key_hex_str.to_owned()),
            std::collections::HashSet::new(),
            None,
        )
        .await
        .expect("second rehydrate must succeed");
        assert_eq!(
            run2.current_step, 0,
            "awaiting_schedule must resume AT the checkpointed step, not past it -- \
             see rehydrate_focus_run()'s resume-index derivation"
        );

        if let Some(v) = saved_root {
            std::env::set_var("QR_DATA_ROOT", v);
        } else {
            std::env::remove_var("QR_DATA_ROOT");
        }
    }

    /// Full completion proof, requiring a real Ollama instance at
    /// OllamaClient's default address (127.0.0.1:21434, QR's own sidecar
    /// port) to actually answer the "quick_response" model call once the gate lets
    /// step_a through. #[ignore]d for the same reason providers::
    /// ocr_eval's live eval tests are: plain `cargo test` (the CI gate)
    /// must not depend on an external service. Run manually with
    /// `cargo test -- --ignored db_composed_focus_schedule_trigger_active_completes_run`.
    #[tokio::test]
    #[ignore]
    async fn db_composed_focus_schedule_trigger_active_completes_run() {
        let _lock = crate::test_support::ENV_MUTEX.lock().await;
        let saved_root = std::env::var("QR_DATA_ROOT").ok();
        let tempdir = tempfile::tempdir().expect("failed to create tempdir");
        std::env::set_var("QR_DATA_ROOT", tempdir.path());

        let user_id = "sched-user-2";
        let persona_id = "sched-persona-2";
        let key_hex_str = RESUME_TEST_KEY_HEX;
        let (pool, topic_id) =
            seed_db_composed_focus_with_schedule_trigger(user_id, persona_id, key_hex_str).await;

        // Anchor already in the past -- trigger.is_active() is true on the
        // very first execute() iteration, so this run never parks at all.
        set_topic_anchor(
            user_id,
            persona_id,
            key_hex_str,
            &topic_id,
            "ship_date",
            Utc::now() - Duration::hours(2),
        )
        .await;

        let focus_run_id = uuid::Uuid::new_v4().to_string();
        {
            let mut conn = open_outputs_db(user_id, persona_id, key_hex_str)
                .await
                .unwrap();
            sqlx::query(
                "INSERT INTO focus_runs
                 (id, focus_id, status, is_fast_lane, is_quick_ask, topic_id, user_input, started_at)
                 VALUES (?, 'sched-focus', 'initializing', 0, 0, ?, 'the scheduler', ?)",
            )
            .bind(&focus_run_id)
            .bind(&topic_id)
            .bind(now())
            .execute(&mut conn)
            .await
            .unwrap();
        }

        let scheduler = Arc::new(ConductorScheduler::new());
        let mut run: FocusRun = rehydrate_focus_run(
            user_id.to_owned(),
            persona_id.to_owned(),
            focus_run_id.clone(),
            pool.clone(),
            scheduler,
            Some(key_hex_str.to_owned()),
            std::collections::HashSet::new(),
            None,
        )
        .await
        .expect("rehydrate_focus_run must succeed");

        let result = run
            .resume_execution()
            .await
            .expect("resume_execution must complete the run against a real local model");
        // Phase 5 OUTPUT lands at 'awaiting_feedback' (Phase 6 FEEDBACK is
        // a separate, later paste-back step, out of scope here) -- the
        // point of this assertion is that it got there at all, through
        // ordinary execute_step()/StepExecutor, not stuck at
        // 'awaiting_schedule' or any other pause.
        assert_eq!(
            result.status, "awaiting_feedback",
            "an already-active schedule_trigger step must run and finish normally, \
             exactly like any plain step -- proves the gate is not a bypass of \
             ordinary execute_step()/StepExecutor"
        );
        assert!(
            result.output_content.is_some(),
            "the real model call must have produced content"
        );

        if let Some(v) = saved_root {
            std::env::set_var("QR_DATA_ROOT", v);
        } else {
            std::env::remove_var("QR_DATA_ROOT");
        }
    }

    // -- items.id=129: block handler registry, LOAD validation, dispatch ----

    /// A migrated shared.db pool for the block LOAD tests. Caller holds the
    /// data-root guard (with_temp_data_root).
    async fn block_load_pool() -> sqlx::SqlitePool {
        crate::persistence::migrations::migrate_shared_db()
            .await
            .expect("shared.db migration must succeed in test setup");
        sqlx::SqlitePool::connect_with(crate::providers::utils::connect_options_unencrypted(
            &crate::providers::utils::db_path_shared(),
        ))
        .await
        .expect("shared.db pool must connect")
    }

    /// One composition-authored Focus with a single block step citing
    /// `block_id`, customization `customization` (raw text, so a test can
    /// store invalid JSON). Foreign keys are switched off on the seeding
    /// connection only, so a row citing an id absent from the mirror can be
    /// stored and the loader's own check is what rejects it.
    async fn seed_block_focus(
        pool: &sqlx::SqlitePool,
        focus_id: &str,
        block_id: &str,
        customization: &str,
    ) {
        let mut conn = pool.acquire().await.unwrap();
        sqlx::query("PRAGMA foreign_keys = OFF")
            .execute(&mut *conn)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO focuses
                (focus_id, display_name, description, version, max_routing_tier,
                 output_type, suggest_in_focuses, multi_source_validation,
                 generic_title_template, status, created_at, updated_at)
             VALUES (?, 'Block Test Focus', '', '1.0', 'local_only',
                     'general', '[]', 0, 'Hidden item', 'shipped',
                     datetime('now'), datetime('now'))",
        )
        .bind(focus_id)
        .execute(&mut *conn)
        .await
        .unwrap();
        let comp_id = format!("comp-{focus_id}");
        sqlx::query(
            "INSERT INTO focus_block_compositions
                (id, focus_id, action_id, block_stable_id, sequence_index,
                 created_at, updated_at)
             VALUES (?, ?, 'quality_check', ?, 0, datetime('now'), datetime('now'))",
        )
        .bind(&comp_id)
        .bind(focus_id)
        .bind(block_id)
        .execute(&mut *conn)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO focus_block_customization
                (id, composition_id, persona_id, customization, created_at, updated_at)
             VALUES (?, ?, NULL, ?, datetime('now'), datetime('now'))",
        )
        .bind(format!("cust-{focus_id}"))
        .bind(&comp_id)
        .bind(customization)
        .execute(&mut *conn)
        .await
        .unwrap();
        drop(conn);
        // A block step may not be last, so every seeded Focus gets an
        // ordinary local step after it.
        seed_plain_step(
            pool,
            focus_id,
            "after_block",
            1,
            serde_json::json!({"routing_tier": 1, "prompt_template": "Refine {draft_output}"}),
        )
        .await;
    }

    /// An ordinary (block_stable_id NULL) composition step at `seq`.
    async fn seed_plain_step(
        pool: &sqlx::SqlitePool,
        focus_id: &str,
        action_id: &str,
        seq: i64,
        customization: serde_json::Value,
    ) {
        let mut conn = pool.acquire().await.unwrap();
        let comp_id = format!("comp-{focus_id}-{action_id}");
        sqlx::query(
            "INSERT INTO focus_block_compositions
                (id, focus_id, action_id, block_stable_id, sequence_index,
                 created_at, updated_at)
             VALUES (?, ?, ?, NULL, ?, datetime('now'), datetime('now'))",
        )
        .bind(&comp_id)
        .bind(focus_id)
        .bind(action_id)
        .bind(seq)
        .execute(&mut *conn)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO focus_block_customization
                (id, composition_id, persona_id, customization, created_at, updated_at)
             VALUES (?, ?, NULL, ?, datetime('now'), datetime('now'))",
        )
        .bind(format!("cust-{focus_id}-{action_id}"))
        .bind(&comp_id)
        .bind(customization.to_string())
        .execute(&mut *conn)
        .await
        .unwrap();
    }

    fn good_cb06_json() -> String {
        serde_json::json!({
            "input_var": "draft_output",
            "output_var": "quality_verdict",
            "criteria": [
                {"name": "has_content", "kind": "non_empty"},
                {"name": "long_enough", "kind": "min_length", "min_length": 10}
            ],
            "on_fail": "advisory"
        })
        .to_string()
    }

    fn load_err_text(r: Result<Option<FocusDefinition>, LifecycleError>) -> String {
        match r {
            Err(LifecycleError::ValidationFailed(m)) => m,
            other => panic!("expected a LOAD ValidationFailed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn block_load_unknown_block_fails_on_mirror_membership() {
        with_temp_data_root(async {
            let pool = block_load_pool().await;
            seed_block_focus(&pool, "unk", "not-a-block", "{}").await;
            let m = load_err_text(load_focus_definition_from_db(&pool, "unk").await);
            assert!(m.contains("'not-a-block'"), "{m}");
            assert!(m.contains("building_blocks mirror"), "{m}");
            assert!(!m.contains("no compiled-in handler"), "{m}");

            // A mirror row that is not inline_composable fails the same check.
            sqlx::query(
                "INSERT INTO building_blocks
                    (stable_id, display_name, invocation_mode, created_at, updated_at)
                 VALUES ('test-gw', 'gateway', 'standing_gateway',
                         datetime('now'), datetime('now'))",
            )
            .execute(&pool)
            .await
            .unwrap();
            seed_block_focus(&pool, "gw", "test-gw", "{}").await;
            let m = load_err_text(load_focus_definition_from_db(&pool, "gw").await);
            assert!(m.contains("building_blocks mirror"), "{m}");
        })
        .await;
    }

    #[tokio::test]
    async fn block_load_cataloged_block_without_handler_fails_at_load() {
        with_temp_data_root(async {
            let pool = block_load_pool().await;
            // cb-02 and cb-10 are in the mirror as inline_composable and have
            // no handler: valid catalog data, not runnable.
            for id in ["cb-02", "cb-10", "cb-01"] {
                let focus = format!("nohandler-{id}");
                seed_block_focus(&pool, &focus, id, "{}").await;
                let m = load_err_text(load_focus_definition_from_db(&pool, &focus).await);
                assert!(m.contains(&format!("'{id}'")), "{m}");
                assert!(m.contains("no compiled-in handler"), "{m}");
                assert!(
                    !m.contains("building_blocks mirror"),
                    "membership and handler errors must be distinct: {m}"
                );
            }
        })
        .await;
    }

    #[tokio::test]
    async fn block_load_bad_cb06_customization_fails_at_load() {
        with_temp_data_root(async {
            let pool = block_load_pool().await;
            let mut with_extra: serde_json::Value =
                serde_json::from_str(&good_cb06_json()).unwrap();
            with_extra["surprise"] = serde_json::json!(true);
            let mut retry: serde_json::Value = serde_json::from_str(&good_cb06_json()).unwrap();
            retry["on_fail"] = serde_json::json!("retry");
            let cases = [
                ("unknown-key", with_extra.to_string()),
                ("on-fail-retry", retry.to_string()),
                ("not-json", "{not json".to_owned()),
                ("empty-object", "{}".to_owned()),
            ];
            for (name, json) in cases {
                let focus = format!("bad-{name}");
                seed_block_focus(&pool, &focus, "cb-06", &json).await;
                let m = load_err_text(load_focus_definition_from_db(&pool, &focus).await);
                assert!(m.contains("customization"), "{name}: {m}");
                assert!(m.contains("quality_check"), "{name}: {m}");
            }
        })
        .await;
    }

    #[tokio::test]
    async fn block_load_good_cb06_loads_as_local_only_block_step() {
        with_temp_data_root(async {
            let pool = block_load_pool().await;
            seed_block_focus(&pool, "good", "cb-06", &good_cb06_json()).await;
            let def = load_focus_definition_from_db(&pool, "good")
                .await
                .unwrap()
                .expect("focus row exists");
            let step = &def.steps[0];
            assert_eq!(step.block_stable_id.as_deref(), Some("cb-06"));
            assert!(step.block_customization.is_some());
            assert_eq!(step.routing_tier, ExternalAccess::LocalOnly);
            assert_eq!(step.external_access_override, None);
            assert!(!step.requires_user_handoff);
        })
        .await;
    }

    #[tokio::test]
    async fn block_load_placement_rules() {
        with_temp_data_root(async {
            let pool = block_load_pool().await;

            // Last step: rejected.
            seed_block_focus(&pool, "last", "cb-06", &good_cb06_json()).await;
            sqlx::query("DELETE FROM focus_block_compositions WHERE id = 'comp-last-after_block'")
                .execute(&pool)
                .await
                .unwrap();
            let m = load_err_text(load_focus_definition_from_db(&pool, "last").await);
            assert!(m.contains("final step"), "{m}");

            // Directly before an external (Tier 2) step: rejected.
            seed_block_focus(&pool, "ext", "cb-06", &good_cb06_json()).await;
            sqlx::query("DELETE FROM focus_block_compositions WHERE id = 'comp-ext-after_block'")
                .execute(&pool)
                .await
                .unwrap();
            seed_plain_step(
                &pool,
                "ext",
                "tier2_step",
                1,
                serde_json::json!({"routing_tier": 2, "prompt_template": "x"}),
            )
            .await;
            let m = load_err_text(load_focus_definition_from_db(&pool, "ext").await);
            assert!(m.contains("external or handoff"), "{m}");

            // Directly before a handoff step: rejected.
            seed_block_focus(&pool, "hand", "cb-06", &good_cb06_json()).await;
            sqlx::query("DELETE FROM focus_block_compositions WHERE id = 'comp-hand-after_block'")
                .execute(&pool)
                .await
                .unwrap();
            seed_plain_step(
                &pool,
                "hand",
                "handoff_step",
                1,
                serde_json::json!({"routing_tier": 1, "requires_user_handoff": true,
                                   "prompt_template": "x"}),
            )
            .await;
            let m = load_err_text(load_focus_definition_from_db(&pool, "hand").await);
            assert!(m.contains("external or handoff"), "{m}");

            // After an external step and before a local one (the Writing
            // Assistant shape): accepted.
            seed_block_focus(&pool, "ok", "cb-06", &good_cb06_json()).await;
            seed_plain_step(
                &pool,
                "ok",
                "tier2_step",
                -1,
                serde_json::json!({"routing_tier": 2, "prompt_template": "x"}),
            )
            .await;
            let def = load_focus_definition_from_db(&pool, "ok")
                .await
                .unwrap()
                .unwrap();
            let ids: Vec<&str> = def.steps.iter().map(|s| s.action_id.as_str()).collect();
            assert_eq!(ids, ["tier2_step", "quality_check", "after_block"]);
        })
        .await;
    }

    #[tokio::test]
    async fn block_step_dispatch_records_verdict_on_the_track() {
        with_temp_data_root(async {
            let pool = block_load_pool().await;
            seed_block_focus(&pool, "disp", "cb-06", &good_cb06_json()).await;
            let def = load_focus_definition_from_db(&pool, "disp")
                .await
                .unwrap()
                .unwrap();
            let step = def.steps[0].clone();

            let mut run = tier3_ready_run(false);
            run.focus_def = Some(def);
            let tt = run.task_track.as_mut().unwrap();
            tt.add_step(TaskStep {
                action_id: "draft_writing".to_owned(),
                output_var: Some("draft_output".to_owned()),
                content: "short".to_owned(),
                sensitivity_severity: 2,
                routing_tier_used: 2,
                personal_flagged: false,
                audit_unavailable: false,
            });

            let failure = run.execute_step(&step, 0).await.expect("block step runs");
            assert!(
                failure.is_none(),
                "an advisory block never reports a failure"
            );

            let tt = run.task_track.as_ref().unwrap();
            assert_eq!(tt.steps().len(), 2);
            // The assessed output is untouched.
            assert_eq!(tt.get_output("draft_output"), Some("short"));
            let verdict = tt.get_output("quality_verdict").expect("verdict recorded");
            assert!(verdict.contains("not passed"), "{verdict}");
            assert!(verdict.contains("long_enough"), "{verdict}");
            assert!(!verdict.contains("short"), "no draft text: {verdict}");
            // The block step does not raise the sensitivity ceiling.
            assert_eq!(tt.sensitivity_ceiling(), 2);
            let recorded = tt.steps().last().unwrap();
            assert_eq!(recorded.action_id, "quality_check");
            assert_eq!(recorded.routing_tier_used, 1);
        })
        .await;
    }

    /// A block ActionDefinition built by hand (the loader's own output shape).
    fn hand_built_cb06_step() -> ActionDefinition {
        let mut step = single_tier3_step_focus_def().steps[0].clone();
        step.action_id = "quality_check".to_owned();
        step.display_name = "quality_check".to_owned();
        step.routing_tier = ExternalAccess::LocalOnly;
        step.external_access_override = None;
        step.requires_user_handoff = false;
        step.block_stable_id = Some("cb-06".to_owned());
        step.block_customization = Some(serde_json::from_str(&good_cb06_json()).unwrap());
        step
    }

    fn draft_step(content: &str) -> TaskStep {
        TaskStep {
            action_id: "draft_writing".to_owned(),
            output_var: Some("draft_output".to_owned()),
            content: content.to_owned(),
            sensitivity_severity: 1,
            routing_tier_used: 2,
            personal_flagged: false,
            audit_unavailable: false,
        }
    }

    #[tokio::test]
    async fn block_step_emits_no_start_or_complete_event() {
        with_temp_data_root(async {
            let mut run = tier3_ready_run(false);
            let mut def = single_tier3_step_focus_def();
            def.steps = vec![hand_built_cb06_step()];
            run.focus_def = Some(def);
            run.task_track
                .as_mut()
                .unwrap()
                .add_step(draft_step("the assessed draft"));

            let r = run.execute().await.expect("execute must not error");
            assert!(r.is_none(), "all steps done, nothing parked: {r:?}");

            // The step ran and is on the track for later steps ...
            let tt = run.task_track.as_ref().unwrap();
            assert!(tt.get_output("quality_verdict").is_some());
            // ... but nothing was emitted for it.
            let emitted = run.emitted_for_test.lock().unwrap();
            assert!(
                emitted.is_empty(),
                "a block step must emit no start or complete event, got {emitted:?}"
            );
        })
        .await;
    }

    /// Control for the test above: the recorder does see an ordinary step's
    /// boundary event, so an empty record for a block step is meaningful.
    #[tokio::test]
    async fn ordinary_step_boundary_is_recorded_by_the_same_recorder() {
        with_temp_data_root(async {
            let mut run = tier3_ready_run(false);
            let r = run.execute().await.unwrap();
            assert_eq!(r.unwrap().status, "awaiting_user");
            let emitted = run.emitted_for_test.lock().unwrap();
            assert!(
                emitted
                    .iter()
                    .any(|p| p.status == "running"
                        && p.step_display_name.as_deref() == Some("Step A")),
                "an ordinary step announces its start: {emitted:?}"
            );
        })
        .await;
    }

    #[test]
    fn block_steps_are_absent_from_the_extraction_input() {
        let mut def = single_tier3_step_focus_def();
        let mut plain_a = def.steps[0].clone();
        plain_a.action_id = "draft_writing".to_owned();
        let mut plain_b = def.steps[0].clone();
        plain_b.action_id = "voice_transform".to_owned();
        def.steps = vec![plain_a, hand_built_cb06_step(), plain_b];

        let mut tt = TaskTrack::new();
        tt.add_step(draft_step("the draft"));
        tt.add_step(TaskStep {
            action_id: "quality_check".to_owned(),
            output_var: Some("quality_verdict".to_owned()),
            content: "Quality check (advisory): passed.".to_owned(),
            sensitivity_severity: 1,
            routing_tier_used: 1,
            personal_flagged: false,
            audit_unavailable: false,
        });
        tt.add_step(TaskStep {
            action_id: "voice_transform".to_owned(),
            output_var: Some("final_output".to_owned()),
            content: "the refined draft".to_owned(),
            sensitivity_severity: 1,
            routing_tier_used: 1,
            personal_flagged: false,
            audit_unavailable: false,
        });

        let out = extraction_step_outputs(&tt, &def);
        assert_eq!(
            out,
            vec![
                ("draft_writing".to_owned(), "the draft".to_owned()),
                ("voice_transform".to_owned(), "the refined draft".to_owned()),
            ]
        );
        // With no block step in the Focus, nothing is filtered.
        let mut plain_def = single_tier3_step_focus_def();
        plain_def.steps[0].action_id = "quality_check".to_owned();
        assert_eq!(extraction_step_outputs(&tt, &plain_def).len(), 3);
    }

    #[tokio::test]
    async fn block_step_with_no_handler_is_a_validation_error_not_a_panic() {
        with_temp_data_root(async {
            let mut run = tier3_ready_run(false);
            let mut step = single_tier3_step_focus_def().steps[0].clone();
            step.block_stable_id = Some("cb-02".to_owned());
            step.block_customization = Some(serde_json::json!({}));
            let err = run.execute_step(&step, 0).await.unwrap_err();
            assert!(
                matches!(&err, LifecycleError::ValidationFailed(m) if m.contains("no compiled-in handler")),
                "{err:?}"
            );
        })
        .await;
    }

    // -- items.id=129: Writing Assistant on the DB composition path ---------

    const WRITING_ASSISTANT_FOCUS: &str =
        include_str!("../../../app/core_artifacts/focuses/writing-assistant.focus");

    /// The seeded writing-assistant row, loaded through the DB path only (no
    /// bundled-file cross-check).
    async fn seeded_wa(pool: &sqlx::SqlitePool) -> FocusDefinition {
        load_focus_definition_from_db(pool, "writing-assistant")
            .await
            .expect("seeded writing-assistant must load")
            .expect("shared_038 seeds a writing-assistant row")
    }

    /// PARITY: the DB-loaded Focus equals the parsed .focus field for field,
    /// plus exactly one extra step (the cb-06 block).
    #[tokio::test]
    async fn writing_assistant_db_focus_matches_the_focus_file_plus_one_block() {
        with_temp_data_root(async {
            let pool = block_load_pool().await;
            let db = seeded_wa(&pool).await;
            let yaml = parse_focus_text(WRITING_ASSISTANT_FOCUS).expect("file parses");

            // Focus level.
            assert_eq!(db.focus_id, yaml.focus_id);
            assert_eq!(db.display_name, yaml.display_name);
            assert_eq!(db.description, yaml.description);
            assert_eq!(db.version, yaml.version);
            assert_eq!(db.max_routing_tier, yaml.max_routing_tier);
            assert_eq!(db.max_routing_tier, ExternalAccess::AnonymousRequired);
            assert_eq!(db.output_type, yaml.output_type);
            assert_eq!(db.suggest_in_focuses, yaml.suggest_in_focuses);
            assert_eq!(db.multi_source_validation, yaml.multi_source_validation);
            assert_eq!(db.generic_title_template, yaml.generic_title_template);
            assert_eq!(
                db.high_priority_trigger.is_some(),
                yaml.high_priority_trigger.is_some()
            );

            // Exactly one extra step, and it is cb-06 between the two.
            assert_eq!(db.steps.len(), yaml.steps.len() + 1);
            let ids: Vec<&str> = db.steps.iter().map(|s| s.action_id.as_str()).collect();
            assert_eq!(
                ids,
                [
                    "voice_analysis",
                    "draft_writing",
                    "quality_check",
                    "voice_transform"
                ]
            );
            let blocks: Vec<&ActionDefinition> = db
                .steps
                .iter()
                .filter(|s| s.block_stable_id.is_some())
                .collect();
            assert_eq!(blocks.len(), 1);
            assert_eq!(blocks[0].action_id, "quality_check");
            assert_eq!(blocks[0].block_stable_id.as_deref(), Some("cb-06"));

            // Every ordinary step, field for field.
            let ordinary: Vec<&ActionDefinition> = db
                .steps
                .iter()
                .filter(|s| s.block_stable_id.is_none())
                .collect();
            assert_eq!(ordinary.len(), yaml.steps.len());
            for (d, y) in ordinary.iter().zip(yaml.steps.iter()) {
                let n = &y.action_id;
                assert_eq!(d.action_id, y.action_id);
                assert_eq!(d.display_name, y.display_name, "{n}");
                assert_eq!(d.guide_id, y.guide_id, "{n}");
                assert_eq!(d.guide_id, "writing-voice", "{n}");
                assert_eq!(d.task_type, y.task_type, "{n}");
                assert_eq!(d.routing_tier, y.routing_tier, "{n}");
                assert_eq!(d.step_type, y.step_type, "{n}");
                assert_eq!(d.output_var, y.output_var, "{n}");
                assert_eq!(d.prompt_template, y.prompt_template, "{n}");
                assert_eq!(d.field_requirements, y.field_requirements, "{n}");
                assert_eq!(d.options_override, y.options_override, "{n}");
                assert_eq!(
                    d.external_access_override, y.external_access_override,
                    "{n}"
                );
                assert_eq!(d.requires_user_handoff, y.requires_user_handoff, "{n}");
                assert_eq!(d.revisitable, y.revisitable, "{n}");
                // items.id=879: the declaration is part of the parity.
                assert_eq!(d.declared_inputs, y.declared_inputs, "{n}");
                assert!(d.block_stable_id.is_none() && d.block_customization.is_none());
                assert!(d.schedule_trigger.is_none() && y.schedule_trigger.is_none());
            }
        })
        .await;
    }

    /// EXTERNAL-STEP PIN: adding cb-06 adds no external behavior and does not
    /// change what voice_transform reads or what becomes the run's output.
    #[tokio::test]
    async fn writing_assistant_external_step_pin() {
        with_temp_data_root(async {
            let pool = block_load_pool().await;
            let db = seeded_wa(&pool).await;
            let yaml = parse_focus_text(WRITING_ASSISTANT_FOCUS).unwrap();

            // Every block step is LocalOnly, no override, no handoff.
            for b in db.steps.iter().filter(|s| s.block_stable_id.is_some()) {
                assert_eq!(b.routing_tier, ExternalAccess::LocalOnly);
                assert!(b.external_access_override.is_none());
                assert!(!b.requires_user_handoff);
            }
            // Step tiers match the YAML: only draft_writing is external.
            for y in &yaml.steps {
                let d = db
                    .steps
                    .iter()
                    .find(|s| s.action_id == y.action_id)
                    .unwrap();
                assert_eq!(d.routing_tier, y.routing_tier, "{}", y.action_id);
            }
            let ext: Vec<&str> = db
                .steps
                .iter()
                .filter(|s| s.routing_tier != ExternalAccess::LocalOnly)
                .map(|s| s.action_id.as_str())
                .collect();
            assert_eq!(ext, ["draft_writing"]);
            // items.id=879: and its declaration, in the seed and in the file.
            for set in [&db, &yaml] {
                let dw = set
                    .steps
                    .iter()
                    .find(|s| s.action_id == "draft_writing")
                    .unwrap();
                assert_eq!(
                    dw.declared_inputs,
                    ["user_input", "voice_analysis_output", "voice_profile"]
                );
            }

            // The block is not directly before an external step and is not last.
            let qi = db
                .steps
                .iter()
                .position(|s| s.block_stable_id.is_some())
                .unwrap();
            let next = &db.steps[qi + 1];
            assert_eq!(next.action_id, "voice_transform");
            assert_eq!(next.routing_tier, ExternalAccess::LocalOnly);
            assert!(qi + 1 < db.steps.len());

            // voice_transform's input resolution is unchanged: it names
            // draft_output explicitly, never previous_output, and nothing
            // reads the verdict variable.
            let vt = db
                .steps
                .iter()
                .find(|s| s.action_id == "voice_transform")
                .unwrap();
            assert!(vt.prompt_template.contains("{draft_output}"));
            assert!(!vt.prompt_template.contains("{previous_output}"));
            let verdict_var = db.steps[qi]
                .block_customization
                .as_ref()
                .and_then(|c| c["output_var"].as_str())
                .unwrap()
                .to_owned();
            for s in &db.steps {
                assert!(
                    !s.prompt_template.contains(&format!("{{{verdict_var}}}")),
                    "{} reads the verdict",
                    s.action_id
                );
            }
            // The verdict never shares an output_var with a real step.
            assert!(db
                .steps
                .iter()
                .filter(|s| s.block_stable_id.is_none())
                .all(|s| s.output_var.as_deref() != Some(verdict_var.as_str())));

            // The run's output (last_output) stays voice_transform's: the
            // block does not become output().
            assert_eq!(db.steps.last().unwrap().action_id, "voice_transform");
            assert_eq!(
                db.steps.last().unwrap().output_var.as_deref(),
                Some("final_output")
            );
        })
        .await;
    }

    /// END TO END over the seeded row: run the real cb-06 step, dispatched
    /// through execute_step, over a track holding the Writing Assistant's real
    /// earlier outputs. The generate steps themselves are covered by parity
    /// and need a model, so a real Tier 2 run stays a by-hand check.
    #[tokio::test]
    async fn writing_assistant_seeded_cb06_step_runs_over_a_seeded_track() {
        with_temp_data_root(async {
            let pool = block_load_pool().await;
            let db = seeded_wa(&pool).await;
            let step = db
                .steps
                .iter()
                .find(|s| s.block_stable_id.is_some())
                .unwrap()
                .clone();

            for (draft, expect_pass) in [
                (
                    "A solid draft that is comfortably longer than forty characters.",
                    true,
                ),
                ("too short", false),
            ] {
                let mut run = tier3_ready_run(false);
                run.focus_def = Some(db.clone());
                let tt = run.task_track.as_mut().unwrap();
                tt.add_step(TaskStep {
                    action_id: "voice_analysis".to_owned(),
                    output_var: Some("voice_analysis_output".to_owned()),
                    content: "analysis".to_owned(),
                    sensitivity_severity: 1,
                    routing_tier_used: 1,
                    personal_flagged: false,
                    audit_unavailable: false,
                });
                tt.add_step(draft_step(draft));

                assert!(run.execute_step(&step, 2).await.unwrap().is_none());

                let tt = run.task_track.as_ref().unwrap();
                assert_eq!(tt.get_output("draft_output"), Some(draft));
                let verdict = tt.get_output("draft_quality_verdict").unwrap();
                assert_eq!(verdict.contains("not passed"), !expect_pass, "{verdict}");
                assert!(!verdict.contains("comfortably"), "no draft text: {verdict}");
                assert!(!verdict.contains("too short"), "no draft text: {verdict}");
                assert_eq!(tt.steps().len(), 3);
            }
        })
        .await;
    }

    /// RUNTIME CROSS-CHECK: load_focus_definition takes the stricter of the DB
    /// row's tier and the bundled file's, for a shipped focus_id.
    #[tokio::test]
    async fn loader_takes_the_stricter_of_db_and_bundled_tier_for_a_shipped_focus() {
        with_temp_data_root(async {
            let pool = block_load_pool().await;

            // Seeded row agrees with the file.
            let def = load_focus_definition(&pool, "writing-assistant").await.unwrap();
            assert_eq!(def.max_routing_tier, ExternalAccess::AnonymousRequired);
            assert_eq!(def.steps.len(), 4);

            // A widened DB row cannot widen the Focus.
            sqlx::query("UPDATE focuses SET max_routing_tier = 'unrestricted' WHERE focus_id = 'writing-assistant'")
                .execute(&pool)
                .await
                .unwrap();
            let def = load_focus_definition(&pool, "writing-assistant").await.unwrap();
            assert_eq!(def.max_routing_tier, ExternalAccess::AnonymousRequired);

            // A stricter DB row narrows it.
            sqlx::query("UPDATE focuses SET max_routing_tier = 'local_only' WHERE focus_id = 'writing-assistant'")
                .execute(&pool)
                .await
                .unwrap();
            let def = load_focus_definition(&pool, "writing-assistant").await.unwrap();
            assert_eq!(def.max_routing_tier, ExternalAccess::LocalOnly);

            // A composition-authored Focus with no bundled file has no .focus
            // reference, but items.id=878's manifest holds it to the most
            // restrictive tier because it has no manifest entry either.
            seed_block_focus(&pool, "no-bundled-file", "cb-06", &good_cb06_json()).await;
            sqlx::query("UPDATE focuses SET max_routing_tier = 'unrestricted' WHERE focus_id = 'no-bundled-file'")
                .execute(&pool)
                .await
                .unwrap();
            let def = load_focus_definition(&pool, "no-bundled-file").await.unwrap();
            assert_eq!(def.max_routing_tier, ExternalAccess::LocalOnly);
        })
        .await;
    }
    // -- items.id=878: guards on composition rows (G1) ----------------------

    /// One composition row for seed_guarded_focus(): (action_id, block id,
    /// guard (var, op, value), customization JSON).
    struct GRow {
        id: String,
        block: Option<&'static str>,
        guard: Option<(&'static str, &'static str, Option<&'static str>)>,
        custom: String,
    }

    /// An ordinary local step recording `out`.
    fn gplain(
        id: &str,
        out: Option<&str>,
        guard: Option<(&'static str, &'static str, Option<&'static str>)>,
    ) -> GRow {
        let mut c = serde_json::json!({"routing_tier": 1, "prompt_template": "x"});
        if let Some(o) = out {
            c["output_var"] = serde_json::json!(o);
        }
        GRow {
            id: id.to_owned(),
            block: None,
            guard,
            custom: c.to_string(),
        }
    }

    /// An ordinary EXTERNAL (tier 2) step.
    fn gext(id: &str) -> GRow {
        GRow {
            id: id.to_owned(),
            block: None,
            guard: None,
            custom: serde_json::json!({"routing_tier": 2, "prompt_template": "x"}).to_string(),
        }
    }

    /// A cb-06 block step recording `out`.
    fn gblock(
        id: &str,
        out: &str,
        guard: Option<(&'static str, &'static str, Option<&'static str>)>,
    ) -> GRow {
        GRow {
            id: id.to_owned(),
            block: Some("cb-06"),
            guard,
            custom: serde_json::json!({
                "input_var": "draft_output",
                "output_var": out,
                "criteria": [{"name": "has_content", "kind": "non_empty"}],
                "on_fail": "advisory"
            })
            .to_string(),
        }
    }

    /// Seeds a Focus whose rows (in order) carry their guard columns.
    async fn seed_guarded_focus(pool: &sqlx::SqlitePool, focus_id: &str, rows: Vec<GRow>) {
        let mut conn = pool.acquire().await.unwrap();
        sqlx::query(
            "INSERT INTO focuses
                (focus_id, display_name, description, version, max_routing_tier,
                 output_type, suggest_in_focuses, multi_source_validation,
                 generic_title_template, status, created_at, updated_at)
             VALUES (?, 'Guard Test Focus', '', '1.0', 'local_only',
                     'general', '[]', 0, 'Hidden item', 'shipped',
                     datetime('now'), datetime('now'))",
        )
        .bind(focus_id)
        .execute(&mut *conn)
        .await
        .unwrap();
        for (seq, row) in rows.iter().enumerate() {
            let comp_id = format!("comp-{focus_id}-{}", row.id);
            let (gv, go, gval) = match row.guard {
                Some((v, o, val)) => (Some(v), Some(o), val),
                None => (None, None, None),
            };
            sqlx::query(
                "INSERT INTO focus_block_compositions
                    (id, focus_id, action_id, block_stable_id, sequence_index,
                     guard_var, guard_op, guard_value, created_at, updated_at)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, datetime('now'), datetime('now'))",
            )
            .bind(&comp_id)
            .bind(focus_id)
            .bind(&row.id)
            .bind(row.block)
            .bind(seq as i64)
            .bind(gv)
            .bind(go)
            .bind(gval)
            .execute(&mut *conn)
            .await
            .unwrap();
            sqlx::query(
                "INSERT INTO focus_block_customization
                    (id, composition_id, persona_id, customization, created_at, updated_at)
                 VALUES (?, ?, NULL, ?, datetime('now'), datetime('now'))",
            )
            .bind(format!("cust-{comp_id}"))
            .bind(&comp_id)
            .bind(&row.custom)
            .execute(&mut *conn)
            .await
            .unwrap();
        }
    }

    async fn guarded_load_err(focus_id: &str, rows: Vec<GRow>) -> String {
        let pool = block_load_pool().await;
        seed_guarded_focus(&pool, focus_id, rows).await;
        load_err_text(load_focus_definition_from_db(&pool, focus_id).await)
    }

    #[tokio::test]
    async fn guard_load_unknown_variable_fails_with_its_own_message() {
        with_temp_data_root(async {
            let m = guarded_load_err(
                "g-unk",
                vec![
                    gplain("a", Some("x"), None),
                    gplain("b", None, Some(("nope", "present", None))),
                ],
            )
            .await;
            assert!(m.contains("step 'b'"), "{m}");
            assert!(m.contains("'nope'"), "{m}");
            assert!(m.contains("unknown variable"), "{m}");
        })
        .await;
    }

    #[tokio::test]
    async fn guard_load_variable_from_a_later_row_is_unknown() {
        with_temp_data_root(async {
            let m = guarded_load_err(
                "g-later",
                vec![
                    gplain("b", None, Some(("x", "present", None))),
                    gplain("a", Some("x"), None),
                ],
            )
            .await;
            assert!(m.contains("unknown variable"), "{m}");
        })
        .await;
    }

    #[tokio::test]
    async fn guard_load_bad_columns_each_have_their_own_message() {
        with_temp_data_root(async {
            let m = guarded_load_err(
                "g-op",
                vec![
                    gplain("a", Some("x"), None),
                    gplain("b", None, Some(("x", "matches", Some("v")))),
                ],
            )
            .await;
            assert!(m.contains("unknown guard operator 'matches'"), "{m}");

            let m = guarded_load_err(
                "g-half",
                vec![
                    gplain("a", Some("x"), None),
                    gplain("b", None, Some(("x", "", None))),
                ],
            )
            .await;
            // An empty-string op is an unknown operator, not NULL.
            assert!(m.contains("unknown guard operator"), "{m}");

            let m = guarded_load_err(
                "g-val",
                vec![
                    gplain("a", Some("x"), None),
                    gplain("b", None, Some(("x", "eq", None))),
                ],
            )
            .await;
            assert!(m.contains("needs a guard_value"), "{m}");

            let m = guarded_load_err(
                "g-in",
                vec![
                    gplain("a", Some("x"), None),
                    gplain("b", None, Some(("x", "in", Some("not json")))),
                ],
            )
            .await;
            assert!(m.contains("not a JSON array"), "{m}");
        })
        .await;
    }

    #[tokio::test]
    async fn guard_load_half_set_guard_columns_fail() {
        with_temp_data_root(async {
            let pool = block_load_pool().await;
            seed_guarded_focus(&pool, "g-half2", vec![gplain("a", Some("x"), None)]).await;
            sqlx::query(
                "UPDATE focus_block_compositions SET guard_var = 'x' WHERE focus_id = 'g-half2'",
            )
            .execute(&pool)
            .await
            .unwrap();
            let m = load_err_text(load_focus_definition_from_db(&pool, "g-half2").await);
            assert!(m.contains("half set"), "{m}");
        })
        .await;
    }

    #[tokio::test]
    async fn guard_load_unreachable_row_fails() {
        with_temp_data_root(async {
            let m = guarded_load_err(
                "g-unreach",
                vec![
                    gplain("a", Some("x"), None),
                    gplain("never", None, Some(("x", "absent", None))),
                    gplain("end", None, None),
                ],
            )
            .await;
            assert!(m.contains("'never' is unreachable"), "{m}");
        })
        .await;
    }

    #[tokio::test]
    async fn guard_load_empty_path_fails() {
        with_temp_data_root(async {
            // A focuses row with no composition rows runs nothing.
            let pool = block_load_pool().await;
            seed_guarded_focus(&pool, "g-empty", vec![]).await;
            let m = load_err_text(load_focus_definition_from_db(&pool, "g-empty").await);
            assert!(m.contains("runs no step"), "{m}");
        })
        .await;
    }

    #[tokio::test]
    async fn guard_load_path_cap_overflow_fails_with_a_clear_message() {
        with_temp_data_root(async {
            let names = ["v0", "v1", "v2", "v3", "v4", "v5", "v6", "v7", "v8"];
            let mut rows: Vec<GRow> = names
                .iter()
                .map(|&n| gplain(&format!("set_{n}"), Some(n), None))
                .collect();
            for n in names {
                rows.push(gplain(&format!("eq_{n}"), None, Some((n, "eq", Some("x")))));
                rows.push(gplain(&format!("ne_{n}"), None, Some((n, "ne", Some("x")))));
            }
            let m = guarded_load_err("g-cap", rows).await;
            assert!(m.contains("more than 256 execution paths"), "{m}");
        })
        .await;
    }

    #[tokio::test]
    async fn guard_load_block_placement_is_checked_per_path() {
        with_temp_data_root(async {
            // On the path where intent is OTHER the guarded local row after the
            // block is skipped, so the next executed row is the external one.
            let m = guarded_load_err(
                "g-place",
                vec![
                    gplain("route", Some("intent"), None),
                    gplain("fb", None, Some(("intent", "present", None))),
                    gblock("blk", "verdict", None),
                    gplain("loc", None, Some(("intent", "eq", Some("a")))),
                    gext("ext"),
                ],
            )
            .await;
            assert!(m.contains("external or handoff step ('ext')"), "{m}");
            assert!(m.contains("intent=OTHER"), "{m}");

            // The same Focus is fine when every path follows the block with a
            // local row.
            let pool = block_load_pool().await;
            seed_guarded_focus(
                &pool,
                "g-place-ok",
                vec![
                    gplain("route", Some("intent"), None),
                    gblock("blk", "verdict", None),
                    gplain("loc", None, Some(("intent", "eq", Some("a")))),
                    gplain("fb", None, Some(("intent", "ne", Some("a")))),
                ],
            )
            .await;
            let def = load_focus_definition_from_db(&pool, "g-place-ok")
                .await
                .unwrap()
                .unwrap();
            assert_eq!(def.steps.len(), 4);
        })
        .await;
    }

    #[tokio::test]
    async fn guard_load_router_without_a_fallback_row_fails() {
        with_temp_data_root(async {
            let m = guarded_load_err(
                "g-nofb",
                vec![
                    gplain("route", Some("intent"), None),
                    gplain("billing", None, Some(("intent", "eq", Some("billing")))),
                    gplain(
                        "tech",
                        None,
                        Some(("intent", "in", Some(r#"["tech","hw"]"#))),
                    ),
                    gplain("end", None, None),
                ],
            )
            .await;
            assert!(
                m.contains("router variable 'intent' has no fallback row"),
                "{m}"
            );
        })
        .await;
    }

    #[tokio::test]
    async fn guard_load_good_router_populates_the_guards_on_the_steps() {
        with_temp_data_root(async {
            let pool = block_load_pool().await;
            seed_guarded_focus(
                &pool,
                "g-ok",
                vec![
                    gplain("route", Some("intent"), None),
                    gplain("billing", None, Some(("intent", "eq", Some("billing")))),
                    gplain("fallback", None, Some(("intent", "ne", Some("billing")))),
                ],
            )
            .await;
            let def = load_focus_definition_from_db(&pool, "g-ok")
                .await
                .unwrap()
                .unwrap();
            assert!(def.steps[0].guard.is_none());
            assert_eq!(
                def.steps[1].guard,
                Some(guard::Guard {
                    var: "intent".to_owned(),
                    condition: guard::Condition::Eq("billing".to_owned()),
                })
            );
            assert_eq!(
                def.steps[2].guard.as_ref().map(|g| &g.condition),
                Some(&guard::Condition::Ne("billing".to_owned()))
            );
            assert_eq!(analyze_guarded_composition("g-ok", &def.steps).unwrap(), 2);
        })
        .await;
    }

    /// A guard on a cb-06 block row reads that block's declared output variable
    /// as an earlier row's output.
    #[tokio::test]
    async fn guard_load_reads_a_blocks_declared_output_var() {
        with_temp_data_root(async {
            let pool = block_load_pool().await;
            seed_guarded_focus(
                &pool,
                "g-blockvar",
                vec![
                    gblock("blk", "verdict", None),
                    gplain("after", None, Some(("verdict", "present", None))),
                ],
            )
            .await;
            let def = load_focus_definition_from_db(&pool, "g-blockvar")
                .await
                .unwrap()
                .unwrap();
            assert_eq!(def.steps.len(), 2);
        })
        .await;
    }

    /// PARITY: the seeded Writing Assistant has no guards and exactly one path.
    #[tokio::test]
    async fn writing_assistant_has_no_guards_and_one_path() {
        with_temp_data_root(async {
            let pool = block_load_pool().await;
            let db = seeded_wa(&pool).await;
            assert!(db.steps.iter().all(|s| s.guard.is_none()));
            assert_eq!(
                analyze_guarded_composition("writing-assistant", &db.steps).unwrap(),
                1
            );
        })
        .await;
    }

    // -- items.id=878: tier manifest through the loader ---------------------

    #[tokio::test]
    async fn manifest_a_db_row_cannot_widen_a_shipped_focus_without_a_focus_file() {
        with_temp_data_root(async {
            let pool = block_load_pool().await;
            // writing-assistant: the manifest and the .focus file agree, and a
            // widened row is held to them. (The .focus file is only found when
            // the dev source dir resolves, so also prove it by the manifest
            // alone through apply_manifest in tier_manifest's own tests.)
            sqlx::query("UPDATE focuses SET max_routing_tier = 'unrestricted' WHERE focus_id = 'writing-assistant'")
                .execute(&pool)
                .await
                .unwrap();
            let def = load_focus_definition(&pool, "writing-assistant").await.unwrap();
            assert_eq!(def.max_routing_tier, ExternalAccess::AnonymousRequired);
        })
        .await;
    }

    #[tokio::test]
    async fn manifest_missing_entry_loads_at_the_most_restrictive_tier() {
        with_temp_data_root(async {
            let pool = block_load_pool().await;
            seed_guarded_focus(&pool, "g-noentry", vec![gplain("a", None, None)]).await;
            sqlx::query(
                "UPDATE focuses SET max_routing_tier = 'unrestricted' WHERE focus_id = 'g-noentry'",
            )
            .execute(&pool)
            .await
            .unwrap();
            assert_eq!(
                crate::conductor::tier_manifest::manifest_tier("g-noentry"),
                None
            );
            let def = load_focus_definition(&pool, "g-noentry").await.unwrap();
            assert_eq!(def.max_routing_tier, ExternalAccess::LocalOnly);
        })
        .await;
    }

    #[tokio::test]
    async fn manifest_a_stricter_row_narrows_a_shipped_focus() {
        with_temp_data_root(async {
            let pool = block_load_pool().await;
            sqlx::query("UPDATE focuses SET max_routing_tier = 'local_only' WHERE focus_id = 'writing-assistant'")
                .execute(&pool)
                .await
                .unwrap();
            let def = load_focus_definition(&pool, "writing-assistant").await.unwrap();
            assert_eq!(def.max_routing_tier, ExternalAccess::LocalOnly);
        })
        .await;
    }

    /// DRIFT: every bundled .focus file has a manifest entry equal to the tier
    /// its YAML gives, and every entry has a bundled file (today all shipped
    /// Focuses are YAML Focuses: relax the second half for a database-only one).
    #[test]
    fn manifest_matches_every_bundled_focus_file() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("app")
            .join("core_artifacts")
            .join("focuses");
        let mut seen = 0;
        for entry in std::fs::read_dir(&dir).expect("bundled focus dir") {
            let path = entry.unwrap().path();
            if path.extension().and_then(|e| e.to_str()) != Some("focus") {
                continue;
            }
            let focus_id = path.file_stem().unwrap().to_str().unwrap().to_owned();
            let parsed = parse_focus_text(&std::fs::read_to_string(&path).unwrap())
                .unwrap_or_else(|e| panic!("{focus_id}: {e}"));
            assert_eq!(
                crate::conductor::tier_manifest::manifest_tier(&focus_id),
                Some(parsed.max_routing_tier),
                "tier manifest entry for '{focus_id}' is missing or differs from its .focus file"
            );
            seen += 1;
        }
        assert!(seen >= 6, "expected the bundled .focus files, saw {seen}");
        for (id, _) in crate::conductor::tier_manifest::SHIPPED_FOCUS_TIERS {
            assert!(
                dir.join(format!("{id}.focus")).exists(),
                "manifest entry '{id}' has no bundled .focus file"
            );
        }
    }

    /// Gate3's look-ahead takes the highest tier a guarded next row could have.
    #[test]
    fn gate3_look_ahead_covers_every_row_that_could_run_next() {
        let step = |tier: ExternalAccess, guarded: bool| {
            let mut s = single_tier3_step_focus_def().steps[0].clone();
            s.routing_tier = tier;
            s.guard = guarded.then(|| {
                guard::Guard::from_columns(Some("v"), Some("present"), None)
                    .unwrap()
                    .unwrap()
            });
            s
        };
        let c = ExternalAccess::Unrestricted;
        // No guards: just the following row, as before.
        let plain = [
            step(ExternalAccess::LocalOnly, false),
            step(ExternalAccess::LocalOnly, false),
            step(ExternalAccess::Unrestricted, false),
        ];
        assert_eq!(next_execution_tier(&plain, 0, c), Some(1));
        assert_eq!(next_execution_tier(&plain, 1, c), Some(3));
        assert_eq!(next_execution_tier(&plain, 2, c), None);
        // A guarded local row next, an unguarded external one after it: either
        // may run next, so the look-ahead sees the external one.
        let guarded = [
            step(ExternalAccess::LocalOnly, false),
            step(ExternalAccess::LocalOnly, true),
            step(ExternalAccess::Unrestricted, false),
            step(ExternalAccess::Unrestricted, false),
        ];
        assert_eq!(next_execution_tier(&guarded, 0, c), Some(3));
        // The scan stops at the first unguarded row.
        let stops = [
            step(ExternalAccess::LocalOnly, false),
            step(ExternalAccess::LocalOnly, false),
            step(ExternalAccess::Unrestricted, false),
        ];
        assert_eq!(next_execution_tier(&stops, 0, c), Some(1));
        // The ceiling still caps it.
        assert_eq!(
            next_execution_tier(&guarded, 0, ExternalAccess::LocalOnly),
            Some(1)
        );
    }

    // -- items.id=878: execute() skips a guarded row ------------------------

    /// A hand-built Focus (bypassing LOAD, like the block tests above) of
    /// model-free work: three cb-06 blocks that run and two ordinary steps whose
    /// guards are false, so a skipped step that WOULD have needed a model must
    /// leave no trace. The draft is on the track already.
    ///
    ///   0 check_a      block, unguarded                 -> quality_verdict
    ///   1 skip_plain   ordinary, guard verdict absent   -> skipped
    ///   2 check_b      block, guard verdict present     -> second_verdict
    ///   3 skip_plain2  ordinary, guard second absent    -> skipped
    ///   4 check_c      block, guard second present      -> third_verdict
    fn guarded_hand_built_def() -> FocusDefinition {
        let block = |id: &str, out: &str, guard: Option<guard::Guard>| {
            let mut s = hand_built_cb06_step();
            s.action_id = id.to_owned();
            s.display_name = id.to_owned();
            let mut cfg: serde_json::Value = serde_json::from_str(&good_cb06_json()).unwrap();
            cfg["output_var"] = serde_json::json!(out);
            s.block_customization = Some(cfg);
            s.guard = guard;
            s
        };
        let plain = |id: &str, guard: guard::Guard| {
            let mut s = single_tier3_step_focus_def().steps[0].clone();
            s.action_id = id.to_owned();
            s.display_name = id.to_owned();
            s.routing_tier = ExternalAccess::LocalOnly;
            s.external_access_override = None;
            s.requires_user_handoff = false;
            s.guard = Some(guard);
            s
        };
        let g = |var: &str, op: &str| {
            guard::Guard::from_columns(Some(var), Some(op), None)
                .unwrap()
                .unwrap()
        };
        let mut def = single_tier3_step_focus_def();
        def.steps = vec![
            block("check_a", "quality_verdict", None),
            plain("skip_plain", g("quality_verdict", "absent")),
            block(
                "check_b",
                "second_verdict",
                Some(g("quality_verdict", "present")),
            ),
            plain("skip_plain2", g("second_verdict", "absent")),
            block(
                "check_c",
                "third_verdict",
                Some(g("second_verdict", "present")),
            ),
        ];
        def
    }

    fn ran_ids(run: &FocusRun<crate::conductor::privacy::logger::TestLogger>) -> Vec<String> {
        run.task_track
            .as_ref()
            .unwrap()
            .steps()
            .iter()
            .map(|s| s.action_id.clone())
            .collect()
    }

    #[tokio::test]
    async fn a_skipped_step_emits_nothing_records_nothing_and_leaves_no_ledger_row() {
        with_temp_data_root(async {
            let mut run = tier3_ready_run(false);
            run.focus_def = Some(guarded_hand_built_def());
            run.task_track
                .as_mut()
                .unwrap()
                .add_step(draft_step("the assessed draft"));
            // The ledger row's FK needs the focus_runs row AUTHORIZE writes.
            crate::persistence::output_store::test_seed_focus_run(
                &run.user_id,
                &run.persona_id,
                CRISIS_TEST_KEY_HEX,
                run.focus_run_id.as_deref().unwrap(),
                "f",
            )
            .await
            .unwrap();

            let r = run.execute().await.expect("execute must not error");
            assert!(r.is_none(), "every row done or skipped: {r:?}");

            // Only the unskipped rows ran, in order, after the seeded draft.
            assert_eq!(
                ran_ids(&run),
                ["draft_writing", "check_a", "check_b", "check_c"]
            );
            // The skipped ordinary steps would have announced themselves.
            assert!(
                run.emitted_for_test.lock().unwrap().is_empty(),
                "a skipped step emits no event: {:?}",
                run.emitted_for_test.lock().unwrap()
            );
            // The track holds no value for a skipped step.
            let tt = run.task_track.as_ref().unwrap();
            assert!(tt.steps().iter().all(|s| !s.action_id.starts_with("skip_")));

            // No focus_run_steps ledger row for a skipped step; one per ran step.
            let focus_run_id = run.focus_run_id.clone().unwrap();
            let mut conn = open_outputs_db(&run.user_id, &run.persona_id, CRISIS_TEST_KEY_HEX)
                .await
                .unwrap();
            let rows: Vec<(String,)> = sqlx::query_as(
                "SELECT action_id FROM focus_run_steps WHERE focus_run_id = ? ORDER BY sequence_index",
            )
            .bind(&focus_run_id)
            .fetch_all(&mut conn)
            .await
            .unwrap();
            let ledger: Vec<&str> = rows.iter().map(|r| r.0.as_str()).collect();
            assert_eq!(ledger, ["check_a", "check_b", "check_c"]);
        })
        .await;
    }

    /// A guard on a variable nothing sets is false: the row is skipped, and so is
    /// every row that reads what it would have recorded.
    #[tokio::test]
    async fn a_guard_on_a_variable_nothing_sets_skips_the_row_and_its_dependents() {
        with_temp_data_root(async {
            let mut run = tier3_ready_run(false);
            let mut def = guarded_hand_built_def();
            def.steps[2].guard = Some(
                guard::Guard::from_columns(Some("never_set"), Some("present"), None)
                    .unwrap()
                    .unwrap(),
            );
            run.focus_def = Some(def);
            run.task_track
                .as_mut()
                .unwrap()
                .add_step(draft_step("the assessed draft"));
            run.execute().await.unwrap();
            assert_eq!(ran_ids(&run), ["draft_writing", "check_a"]);
        })
        .await;
    }

    /// Checkpoints are written only for steps that ran. A crash after any of
    /// them resumes on the same path: the guards between the checkpoint and the
    /// next executed step are evaluated over the restored track and agree with
    /// the uninterrupted run.
    #[tokio::test]
    async fn resume_after_a_checkpoint_stays_on_the_same_path_as_the_uninterrupted_run() {
        with_temp_data_root(async {
            let saved = std::env::var("QR_CHECKPOINT_EVERY_N_STEPS").ok();
            std::env::set_var("QR_CHECKPOINT_EVERY_N_STEPS", "1");

            let full = guarded_hand_built_def();

            // The uninterrupted run, as the reference.
            let mut reference = tier3_ready_run(false);
            reference.focus_def = Some(full.clone());
            reference
                .task_track
                .as_mut()
                .unwrap()
                .add_step(draft_step("the assessed draft"));
            reference.execute().await.unwrap();
            let expected = ran_ids(&reference);
            assert_eq!(expected, ["draft_writing", "check_a", "check_b", "check_c"]);

            // Interrupt after each executed row k (a crash right after its
            // checkpoint), including the ones directly before a skipped row.
            for crash_after in 0..full.steps.len() {
                let mut first = tier3_ready_run(false);
                let mut truncated = full.clone();
                truncated.steps.truncate(crash_after + 1);
                first.focus_def = Some(truncated);
                first
                    .task_track
                    .as_mut()
                    .unwrap()
                    .add_step(draft_step("the assessed draft"));
                let focus_run_id = first.focus_run_id.clone().unwrap();
                crate::persistence::output_store::test_seed_focus_run(
                    &first.user_id,
                    &first.persona_id,
                    CRISIS_TEST_KEY_HEX,
                    &focus_run_id,
                    "f",
                )
                .await
                .unwrap();
                first.execute().await.unwrap();

                let mut conn =
                    open_outputs_db(&first.user_id, &first.persona_id, CRISIS_TEST_KEY_HEX)
                        .await
                        .unwrap();
                let snap = sqlx::query(
                    "SELECT action_id, task_track_json FROM focus_run_snapshots
                     WHERE focus_run_id = ? ORDER BY created_at DESC, rowid DESC LIMIT 1",
                )
                .bind(&focus_run_id)
                .fetch_optional(&mut conn)
                .await
                .unwrap();
                drop(conn);

                // The resumed run, exactly as rehydrate_focus_run() builds it.
                let mut resumed = tier3_ready_run(false);
                resumed.focus_def = Some(full.clone());
                match snap {
                    Some(row) => {
                        let action_id: String = row.try_get("action_id").unwrap();
                        let json: String = row.try_get("task_track_json").unwrap();
                        // A checkpoint is never keyed to a skipped step.
                        assert!(!action_id.starts_with("skip_"), "{action_id}");
                        resumed.task_track = Some(deserialize_task_track(&json).unwrap());
                        resumed.current_step = resume_index(&full.steps, &action_id, "running");
                    }
                    None => {
                        // Nothing checkpointed yet (the crash came before any
                        // executed row): a fresh start.
                        resumed
                            .task_track
                            .as_mut()
                            .unwrap()
                            .add_step(draft_step("the assessed draft"));
                    }
                }
                resumed.execute().await.unwrap();
                assert_eq!(
                    ran_ids(&resumed),
                    expected,
                    "crash after row {crash_after} resumed on a different path"
                );
            }

            match saved {
                Some(v) => std::env::set_var("QR_CHECKPOINT_EVERY_N_STEPS", v),
                None => std::env::remove_var("QR_CHECKPOINT_EVERY_N_STEPS"),
            }
        })
        .await;
    }

    /// Re-entering at an earlier step rebuilds the track without the later
    /// outputs, so a later row's guard is evaluated over the REBUILT track and
    /// the rows after the re-entry point run again under it.
    #[tokio::test]
    async fn reentry_re_evaluates_guards_on_the_rebuilt_track() {
        with_temp_data_root(async {
            let full = guarded_hand_built_def();
            let order: Vec<String> = full.steps.iter().map(|s| s.action_id.clone()).collect();

            let mut run = tier3_ready_run(false);
            run.focus_def = Some(full.clone());
            run.task_track
                .as_mut()
                .unwrap()
                .add_step(draft_step("the assessed draft"));
            let focus_run_id = run.focus_run_id.clone().unwrap();
            crate::persistence::output_store::test_seed_focus_run(
                &run.user_id,
                &run.persona_id,
                CRISIS_TEST_KEY_HEX,
                &focus_run_id,
                "f",
            )
            .await
            .unwrap();
            run.execute().await.unwrap();
            let original = ran_ids(&run);
            assert_eq!(original, ["draft_writing", "check_a", "check_b", "check_c"]);

            // check_c's guard (second_verdict present) holds on the finished
            // track, skip_plain2's (second_verdict absent) does not.
            let finished = guard::track_vars(run.task_track.as_ref().unwrap());
            assert!(full.steps[4].guard.as_ref().unwrap().evaluate(&finished));
            assert!(!full.steps[3].guard.as_ref().unwrap().evaluate(&finished));

            run.reenter_step(&order, "check_b")
                .await
                .expect("re-entry at check_b");
            assert_eq!(run.current_step, 2);
            assert_eq!(ran_ids(&run), ["draft_writing", "check_a"]);

            // On the rebuilt track the answers flip.
            let rebuilt = guard::track_vars(run.task_track.as_ref().unwrap());
            assert!(!full.steps[4].guard.as_ref().unwrap().evaluate(&rebuilt));
            assert!(full.steps[3].guard.as_ref().unwrap().evaluate(&rebuilt));

            // Running forward from the re-entry point re-evaluates them live and
            // lands on the same path again.
            run.execute().await.unwrap();
            assert_eq!(ran_ids(&run), original);
        })
        .await;
    }

    // -- items.id=886: bound names, seeding, taint, interim invariant --------

    /// An ordinary LOCAL step with `template`, recording `out`.
    fn local_step(id: &str, template: &str, out: Option<&str>) -> ActionDefinition {
        let mut s = single_tier3_step_focus_def().steps[0].clone();
        s.action_id = id.to_owned();
        s.display_name = id.to_owned();
        s.routing_tier = ExternalAccess::LocalOnly;
        s.external_access_override = None;
        s.requires_user_handoff = false;
        s.prompt_template = template.to_owned();
        s.output_var = out.map(str::to_owned);
        s
    }

    fn recorded(id: &str, out: Option<&str>, content: &str) -> TaskStep {
        TaskStep {
            action_id: id.to_owned(),
            output_var: out.map(str::to_owned),
            content: content.to_owned(),
            sensitivity_severity: 1,
            routing_tier_used: 1,
            personal_flagged: false,
            audit_unavailable: false,
        }
    }

    /// Direct taint: a step whose template names a bound name is dropped from
    /// the extraction input, and so is nothing else.
    #[test]
    fn extraction_drops_a_step_that_names_a_bound_name() {
        let mut def = single_tier3_step_focus_def();
        def.steps = vec![
            local_step("clean", "{user_input}", Some("c")),
            local_step("direct", "{case_context}", Some("d")),
        ];
        let mut tt = TaskTrack::new();
        tt.add_step(recorded("clean", Some("c"), "clean content"));
        tt.add_step(recorded("direct", Some("d"), "CASE CONTENT"));
        assert_eq!(
            extraction_step_outputs(&tt, &def),
            vec![("clean".to_owned(), "clean content".to_owned())]
        );
    }

    /// Transitive taint: a step naming a tainted variable is dropped, through
    /// any number of hops, whatever the row order, and so is a step that names
    /// {previous_output} after a tainted row.
    #[test]
    fn extraction_drops_steps_that_name_a_tainted_variable_transitively() {
        let mut def = single_tier3_step_focus_def();
        def.steps = vec![
            local_step("hop3", "polish {v2}", Some("v3")),
            local_step("root", "{locked_intent}", Some("v1")),
            local_step("hop2", "summarise {v1}", Some("v2")),
            local_step("carry", "{previous_output}", Some("v4")),
            local_step("clean", "{user_input}", Some("c")),
        ];
        let mut tt = TaskTrack::new();
        for (id, out) in [
            ("root", "v1"),
            ("hop2", "v2"),
            ("hop3", "v3"),
            ("carry", "v4"),
            ("clean", "c"),
        ] {
            tt.add_step(recorded(id, Some(out), &format!("{id} content")));
        }
        assert_eq!(
            extraction_step_outputs(&tt, &def),
            vec![("clean".to_owned(), "clean content".to_owned())]
        );
    }

    #[test]
    fn extraction_is_unchanged_when_no_step_names_a_bound_name() {
        let mut def = single_tier3_step_focus_def();
        def.steps = vec![
            local_step("a", "{user_input} {persona_context}", Some("v1")),
            local_step("b", "{v1} {previous_output}", Some("v2")),
        ];
        let mut tt = TaskTrack::new();
        tt.add_step(recorded("a", Some("v1"), "one"));
        tt.add_step(recorded("b", Some("v2"), "two"));
        assert_eq!(extraction_step_outputs(&tt, &def).len(), 2);
    }

    /// A Focus with a guard on a bound name and two exclusive block rows.
    fn write_mode_def() -> FocusDefinition {
        let block = |id: &str, mode: &str| {
            let mut s = hand_built_cb06_step();
            s.action_id = id.to_owned();
            s.display_name = id.to_owned();
            s.guard =
                guard::Guard::from_columns(Some("write_mode"), Some("eq"), Some(mode)).unwrap();
            s
        };
        let mut def = single_tier3_step_focus_def();
        def.steps = vec![
            block("open_check", "open"),
            block("held_check", "suppressed"),
        ];
        def
    }

    /// No chat means write_mode cannot be read: the pass fails closed and no
    /// step runs. It does not fall back to open.
    #[tokio::test]
    async fn execute_fails_closed_when_write_mode_cannot_be_read() {
        with_temp_data_root(async {
            let mut run = tier3_ready_run(false);
            run.focus_def = Some(write_mode_def());
            run.context_key = None;
            run.task_track
                .as_mut()
                .unwrap()
                .add_step(draft_step("the assessed draft"));
            let err = run.execute().await.unwrap_err();
            assert!(
                matches!(
                    &err,
                    LifecycleError::StoreSeed(crate::conductor::seeding::SeedError::NoContextKey(
                        "write_mode"
                    ))
                ),
                "{err:?}"
            );
            assert_eq!(ran_ids(&run), ["draft_writing"], "no step ran");
        })
        .await;
    }

    /// The seed reaches the guard: the open row runs while the chat is open, the
    /// suppressed row once the one-time latch is set, read at pass start from
    /// the real encrypted outputs.db.
    #[tokio::test]
    async fn execute_seeds_write_mode_from_the_store_and_the_guard_follows_it() {
        with_temp_data_root(async {
            let make_run = || {
                let mut run = tier3_ready_run(false);
                run.focus_def = Some(write_mode_def());
                run.context_key = Some("chat-wm".to_owned());
                run.task_track
                    .as_mut()
                    .unwrap()
                    .add_step(draft_step("the assessed draft"));
                run
            };

            let mut run = make_run();
            run.execute().await.unwrap();
            assert_eq!(ran_ids(&run), ["draft_writing", "open_check"]);
            assert_eq!(
                run.store_tokens.seeds.get("write_mode").map(String::as_str),
                Some("open")
            );

            let mut conn = open_outputs_db(&run.user_id, &run.persona_id, CRISIS_TEST_KEY_HEX)
                .await
                .unwrap();
            crate::persistence::session_store::latch_write_suppressed(
                &mut conn, "chat-wm", "f", "run-x",
            )
            .await
            .unwrap();
            drop(conn);

            let mut latched = make_run();
            latched.execute().await.unwrap();
            assert_eq!(ran_ids(&latched), ["draft_writing", "held_check"]);
            assert_eq!(
                latched
                    .store_tokens
                    .seeds
                    .get("write_mode")
                    .map(String::as_str),
                Some("suppressed")
            );

            // Another chat is still open: the latch is per chat.
            let mut other = make_run();
            other.context_key = Some("chat-other".to_owned());
            other.execute().await.unwrap();
            assert_eq!(ran_ids(&other), ["draft_writing", "open_check"]);
        })
        .await;
    }

    /// A Focus that names no bound name never touches the store: no chat is
    /// needed and no database is opened (the data root stays empty).
    #[tokio::test]
    async fn a_focus_with_no_bound_name_reads_nothing() {
        with_temp_data_root(async {
            let mut run = tier3_ready_run(false);
            run.key_hex = None; // would fail an open, so none may be attempted
            run.context_key = None;
            run.focus_def = Some({
                let mut d = single_tier3_step_focus_def();
                d.steps = vec![hand_built_cb06_step()];
                d
            });
            run.task_track
                .as_mut()
                .unwrap()
                .add_step(draft_step("the assessed draft"));
            run.execute().await.unwrap();
            assert!(run.store_tokens.seeds.is_empty());
        })
        .await;
    }

    // LOAD (database path): the interim invariant, collisions, guards.

    #[tokio::test]
    async fn store_names_db_an_external_step_cannot_name_or_declare_one() {
        with_temp_data_root(async {
            // Declared, so the declared-inputs rule is met: the interim
            // invariant is what refuses it.
            let m = declared_load(
                "sn-declared",
                vec![dstep(
                    "e",
                    2,
                    "{case_context}",
                    &["case_context"],
                    &[],
                    None,
                )],
            )
            .await
            .unwrap_err();
            assert!(m.contains("Focus 'sn-declared'"), "{m}");
            assert!(
                m.contains("step 'e'") && m.contains("'case_context'"),
                "{m}"
            );
            assert!(m.contains("items.id=884"), "{m}");

            // Undeclared: the declared-inputs rule speaks first.
            let m = declared_load(
                "sn-undeclared",
                vec![dstep("e", 2, "{delta_context}", &[], &[], None)],
            )
            .await
            .unwrap_err();
            assert!(m.contains("session-bound"), "{m}");

            // Transitive: an external step naming a variable a local step
            // derived from a store name.
            let m = declared_load(
                "sn-transitive",
                vec![
                    dstep("a", 1, "{locked_intent}", &[], &[], Some("echo1")),
                    dstep("b", 1, "{echo1}", &[], &[], Some("echo2")),
                    dstep("e", 2, "{echo2}", &["echo2"], &[], None),
                ],
            )
            .await
            .unwrap_err();
            assert!(m.contains("step 'e'") && m.contains("'echo2'"), "{m}");
            assert!(m.contains("items.id=884"), "{m}");
        })
        .await;
    }

    #[tokio::test]
    async fn store_names_db_local_steps_and_clean_external_steps_still_load() {
        with_temp_data_root(async {
            declared_load(
                "sn-local",
                vec![
                    dstep(
                        "a",
                        1,
                        "{case_context} {delta_context} {write_mode}",
                        &["case_context"],
                        &[],
                        Some("echo1"),
                    ),
                    dstep("e", 2, "{user_input}", &["user_input"], &[], None),
                ],
            )
            .await
            .unwrap();
        })
        .await;
    }

    #[tokio::test]
    async fn store_names_db_an_output_var_cannot_be_a_bound_name() {
        with_temp_data_root(async {
            let m = declared_load(
                "sn-collide",
                vec![dstep("a", 1, "x", &[], &[], Some("case_context"))],
            )
            .await
            .unwrap_err();
            assert!(
                m.contains("step 'a'") && m.contains("output_var 'case_context'"),
                "{m}"
            );
        })
        .await;
    }

    #[tokio::test]
    async fn store_names_db_a_guard_on_a_bound_name_loads_and_is_path_checked() {
        with_temp_data_root(async {
            let pool = block_load_pool().await;
            seed_guarded_focus(
                &pool,
                "sn-guard",
                vec![
                    gplain("a", None, None),
                    gplain("b", None, Some(("write_mode", "eq", Some("open")))),
                    gplain("c", None, Some(("write_mode", "eq", Some("suppressed")))),
                ],
            )
            .await;
            let def = load_focus_definition_from_db(&pool, "sn-guard")
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                analyze_guarded_composition("sn-guard", &def.steps).unwrap(),
                2
            );

            // A literal outside write_mode's closed domain leaves its row
            // unreachable, which LOAD refuses.
            seed_guarded_focus(
                &pool,
                "sn-guard-typo",
                vec![
                    gplain("a", None, None),
                    gplain("b", None, Some(("write_mode", "eq", Some("supressed")))),
                ],
            )
            .await;
            let m = load_err_text(load_focus_definition_from_db(&pool, "sn-guard-typo").await);
            assert!(m.contains("unreachable") && m.contains("'b'"), "{m}");
        })
        .await;
    }

    /// The YAML loader runs the same lint, so a Focus file cannot smuggle a
    /// store name to an external step either.
    #[test]
    fn store_names_yaml_an_external_step_cannot_name_one() {
        let text = r#"
id: sn-yaml
max_routing_tier: 2
steps:
  first:
    routing_tier: 1
    output_var: v
    prompt_template: "{case_context}"
  second:
    routing_tier: 2
    prompt_template: "use {v}"
    declared_inputs:
      - v
"#;
        let def = parse_focus_text(text).unwrap();
        let LifecycleError::ValidationFailed(m) =
            lint_declared_inputs(&def.focus_id, &def.steps).unwrap_err()
        else {
            panic!("expected ValidationFailed");
        };
        assert!(m.contains("step 'second'") && m.contains("'v'"), "{m}");
        assert!(m.contains("items.id=884"), "{m}");
    }

    // -- items.id=879: declared-inputs LOAD lint -----------------------------

    /// An ordinary step at `tier` with a template, a declaration and field names.
    fn dstep(
        id: &str,
        tier: u8,
        template: &str,
        declared: &[&str],
        fields: &[&str],
        out: Option<&str>,
    ) -> GRow {
        let mut c = serde_json::json!({
            "routing_tier": tier,
            "prompt_template": template,
            "declared_inputs": declared,
            "field_requirements": fields
                .iter()
                .map(|f| serde_json::json!({"name": f, "scope": "optional"}))
                .collect::<Vec<_>>(),
        });
        if let Some(o) = out {
            c["output_var"] = serde_json::json!(o);
        }
        GRow {
            id: id.to_owned(),
            block: None,
            guard: None,
            custom: c.to_string(),
        }
    }

    async fn declared_load(focus_id: &str, rows: Vec<GRow>) -> Result<(), String> {
        let pool = block_load_pool().await;
        seed_guarded_focus(&pool, focus_id, rows).await;
        match load_focus_definition_from_db(&pool, focus_id).await {
            Ok(_) => Ok(()),
            Err(LifecycleError::ValidationFailed(m)) => Err(m),
            Err(other) => panic!("unexpected error {other:?}"),
        }
    }

    #[tokio::test]
    async fn declared_inputs_db_undeclared_variable_in_an_external_template_fails_load() {
        with_temp_data_root(async {
            let m = declared_load(
                "di-undeclared",
                vec![
                    dstep("a", 1, "x", &[], &[], Some("v")),
                    dstep("e", 2, "use {v}", &[], &[], None),
                ],
            )
            .await
            .unwrap_err();
            assert!(m.contains("Focus 'di-undeclared'"), "{m}");
            assert!(m.contains("step 'e'") && m.contains("{v}"), "{m}");
            assert!(m.contains("does not declare"), "{m}");
        })
        .await;
    }

    #[tokio::test]
    async fn declared_inputs_db_a_declared_name_loads() {
        with_temp_data_root(async {
            declared_load(
                "di-declared",
                vec![
                    dstep("a", 1, "x", &[], &[], Some("v")),
                    dstep(
                        "e",
                        2,
                        "use {v} {user_input}",
                        &["v", "user_input"],
                        &[],
                        None,
                    ),
                ],
            )
            .await
            .unwrap();
        })
        .await;
    }

    #[tokio::test]
    async fn declared_inputs_db_a_field_requirements_name_is_implicitly_declared() {
        with_temp_data_root(async {
            declared_load(
                "di-field",
                vec![dstep("e", 2, "{job_title}", &[], &["job_title"], None)],
            )
            .await
            .unwrap();
        })
        .await;
    }

    #[tokio::test]
    async fn declared_inputs_db_a_local_step_needs_no_declaration() {
        with_temp_data_root(async {
            declared_load(
                "di-local",
                vec![
                    dstep("a", 1, "x", &[], &[], Some("v")),
                    dstep("l", 1, "{v} {persona_context} {user_input}", &[], &[], None),
                ],
            )
            .await
            .unwrap();
        })
        .await;
    }

    #[tokio::test]
    async fn declared_inputs_db_a_declared_name_nothing_produces_fails_load() {
        with_temp_data_root(async {
            let m = declared_load("di-ghost", vec![dstep("e", 2, "x", &["ghost"], &[], None)])
                .await
                .unwrap_err();
            assert!(m.contains("declared_inputs names 'ghost'"), "{m}");
        })
        .await;
    }

    #[tokio::test]
    async fn declared_inputs_db_a_step_after_a_block_cannot_name_the_block_variable_undeclared() {
        with_temp_data_root(async {
            let rows = |declared: &[&str]| {
                vec![
                    dstep("draft", 1, "x", &[], &[], Some("draft_output")),
                    gblock("qc", "draft_quality_verdict", None),
                    dstep(
                        "tail",
                        1,
                        "Refine {draft_output}",
                        &[],
                        &[],
                        Some("tail_out"),
                    ),
                    dstep("e", 2, "{draft_quality_verdict}", declared, &[], None),
                ]
            };
            let m = declared_load("di-block-undeclared", rows(&[]))
                .await
                .unwrap_err();
            assert!(
                m.contains("{draft_quality_verdict}") && m.contains("does not declare"),
                "{m}"
            );
            declared_load("di-block-declared", rows(&["draft_quality_verdict"]))
                .await
                .unwrap();
        })
        .await;
    }

    /// The lint runs before the block-placement analysis: an external step
    /// directly after a block fails as an undeclared name, not as placement.
    #[tokio::test]
    async fn declared_inputs_db_lint_message_wins_over_placement() {
        with_temp_data_root(async {
            let m = declared_load(
                "di-order",
                vec![
                    dstep("draft", 1, "x", &[], &[], Some("draft_output")),
                    gblock("qc", "draft_quality_verdict", None),
                    dstep("e", 2, "{draft_quality_verdict}", &[], &[], None),
                ],
            )
            .await
            .unwrap_err();
            assert!(m.contains("does not declare"), "{m}");
        })
        .await;
    }

    // Shipped Focuses ---------------------------------------------------------

    const SHIPPED_FOCUS_IDS: [&str; 6] = [
        "quick-ask",
        "writing-assistant",
        "research-and-buy",
        "role-assessment",
        "api-chat",
        "qr-admin",
    ];

    fn shipped_focus_text(id: &str) -> String {
        std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../app/core_artifacts/focuses")
                .join(format!("{id}.focus")),
        )
        .unwrap()
    }

    /// (step, external) pairs: the steps each shipped Focus reaches externally.
    /// Pinned so that declaring inputs makes nothing newly external or reachable.
    fn shipped_external_steps(id: &str) -> &'static [&'static str] {
        match id {
            "writing-assistant" => &["draft_writing"],
            "research-and-buy" => &["research", "recommendation"],
            "role-assessment" => &["fit_analysis", "salary_research", "disposition"],
            "api-chat" => &["reply"],
            _ => &[],
        }
    }

    #[tokio::test]
    async fn declared_inputs_every_shipped_focus_loads_with_its_declarations() {
        with_temp_data_root(async {
            let pool = block_load_pool().await;
            // load_focus_definition: writing-assistant takes the DB path (its
            // shared_038/040 seed rows), the other five take the YAML path.
            for id in SHIPPED_FOCUS_IDS {
                let def = load_focus_definition(&pool, id)
                    .await
                    .unwrap_or_else(|e| panic!("{id} must load: {e}"));
                let ext: Vec<&str> = def
                    .steps
                    .iter()
                    .filter(|s| s.routing_tier != ExternalAccess::LocalOnly)
                    .map(|s| s.action_id.as_str())
                    .collect();
                assert_eq!(ext, shipped_external_steps(id), "{id}: external steps");
            }
            // writing-assistant's YAML file also passes the YAML-path lint
            // (its DB row shadows it in load_focus_definition above).
            let yaml = parse_focus_text(&shipped_focus_text("writing-assistant")).unwrap();
            lint_declared_inputs(&yaml.focus_id, &yaml.steps).unwrap();
        })
        .await;
    }

    /// Nothing is papered over: for every shipped external step the declaration
    /// plus its field_requirements names equals the names find_tokens finds in
    /// its template (nothing missing, nothing over-declared).
    #[tokio::test]
    async fn declared_inputs_match_exactly_what_each_shipped_template_uses() {
        with_temp_data_root(async {
            let pool = block_load_pool().await;
            let mut defs: Vec<(String, FocusDefinition)> = SHIPPED_FOCUS_IDS
                .iter()
                .map(|id| {
                    (
                        (*id).to_owned(),
                        parse_focus_text(&shipped_focus_text(id)).unwrap(),
                    )
                })
                .collect();
            // And the seeded database row for writing-assistant.
            defs.push(("writing-assistant(db)".to_owned(), seeded_wa(&pool).await));
            for (id, def) in &defs {
                for s in def
                    .steps
                    .iter()
                    .filter(|s| s.block_stable_id.is_none())
                    .filter(|s| s.routing_tier != ExternalAccess::LocalOnly)
                {
                    let used: std::collections::BTreeSet<String> =
                        crate::conductor::executor::find_tokens(&s.prompt_template)
                            .into_iter()
                            .collect();
                    let declared: std::collections::BTreeSet<String> =
                        s.declared_inputs.iter().cloned().collect();
                    let fields: std::collections::BTreeSet<String> =
                        s.field_requirements.keys().cloned().collect();
                    let covered: std::collections::BTreeSet<String> =
                        declared.union(&fields).cloned().collect();
                    assert_eq!(used, covered, "{id}/{}", s.action_id);
                    // No name is declared that is also a field (no double entry).
                    assert!(declared.is_disjoint(&fields), "{id}/{}", s.action_id);
                }
            }
        })
        .await;
    }

    /// Each shipped external declaration is load-bearing: removing it fails the
    /// lint on that step.
    #[test]
    fn declared_inputs_removing_a_shipped_declaration_fails_the_lint() {
        for id in SHIPPED_FOCUS_IDS {
            let def = parse_focus_text(&shipped_focus_text(id)).unwrap();
            for (i, s) in def.steps.iter().enumerate() {
                if s.declared_inputs.is_empty() {
                    continue;
                }
                let mut steps = def.steps.clone();
                steps[i].declared_inputs.clear();
                let err = lint_declared_inputs(id, &steps).unwrap_err();
                let LifecycleError::ValidationFailed(m) = err else {
                    panic!("expected ValidationFailed");
                };
                assert!(m.contains(&format!("step '{}'", s.action_id)), "{id}: {m}");
            }
        }
    }

    #[test]
    fn declared_inputs_yaml_parses_the_field_and_the_lint_runs_on_that_path() {
        let text = r#"
id: di-yaml
max_routing_tier: 2
steps:
  first:
    routing_tier: 1
    output_var: v
    prompt_template: "x"
  second:
    routing_tier: 2
    prompt_template: "use {v}"
    declared_inputs:
      - v
"#;
        let def = parse_focus_text(text).unwrap();
        assert_eq!(def.steps[0].declared_inputs, Vec::<String>::new());
        assert_eq!(def.steps[1].declared_inputs, ["v"]);
        lint_declared_inputs("di-yaml", &def.steps).unwrap();

        let undeclared = text.replace("    declared_inputs:\n      - v\n", "");
        let def = parse_focus_text(&undeclared).unwrap();
        let LifecycleError::ValidationFailed(m) =
            lint_declared_inputs("di-yaml", &def.steps).unwrap_err()
        else {
            panic!("expected ValidationFailed");
        };
        assert!(m.contains("step 'second'") && m.contains("{v}") && m.contains("does not declare"));
    }

    /// shared_038 seeded draft_writing WITHOUT a declaration. An existing
    /// install holding that row fails the lint (which is why shared_040 exists),
    /// and the repaired row loads.
    #[tokio::test]
    async fn declared_inputs_a_stale_writing_assistant_seed_row_fails_and_the_repair_loads() {
        with_temp_data_root(async {
            let pool = block_load_pool().await;
            sqlx::query(
                "UPDATE focus_block_customization \
                 SET customization = json_remove(customization, '$.declared_inputs') \
                 WHERE id = 'writing-assistant:draft_writing:default'",
            )
            .execute(&pool)
            .await
            .unwrap();
            let err = load_focus_definition_from_db(&pool, "writing-assistant")
                .await
                .unwrap_err();
            let LifecycleError::ValidationFailed(m) = err else {
                panic!("expected ValidationFailed");
            };
            assert!(
                m.contains("step 'draft_writing'") && m.contains("does not declare"),
                "{m}"
            );

            sqlx::query(
                "UPDATE focus_block_customization \
                 SET customization = json_set(customization, '$.declared_inputs', \
                     json('[\"user_input\",\"voice_analysis_output\",\"voice_profile\"]')) \
                 WHERE id = 'writing-assistant:draft_writing:default'",
            )
            .execute(&pool)
            .await
            .unwrap();
            assert!(load_focus_definition_from_db(&pool, "writing-assistant")
                .await
                .is_ok());
        })
        .await;
    }

    // -- items.id=887: store writers on composition rows --------------------

    /// One composition row for seed_store_focus().
    struct SRow {
        id: &'static str,
        op: Option<&'static str>,
        target: Option<&'static str>,
        args: Option<&'static str>,
        guard: Option<(&'static str, &'static str, Option<&'static str>)>,
        block: Option<&'static str>,
        custom: String,
        trigger: bool,
        /// items.id=889: the raw guard_clauses column (a JSON array), or None.
        clauses: Option<String>,
    }

    /// An ordinary local step recording `out`.
    fn smodel(id: &'static str, out: &str) -> SRow {
        SRow {
            id,
            op: None,
            target: None,
            args: None,
            guard: None,
            block: None,
            custom: serde_json::json!({
                "routing_tier": 1, "prompt_template": "x", "output_var": out
            })
            .to_string(),
            trigger: false,
            clauses: None,
        }
    }

    /// A store writer row: empty customization, as LOAD requires.
    fn swriter(
        id: &'static str,
        op: &'static str,
        target: Option<&'static str>,
        args: Option<&'static str>,
    ) -> SRow {
        SRow {
            id,
            op: Some(op),
            target,
            args,
            guard: None,
            block: None,
            custom: "{}".to_owned(),
            trigger: false,
            clauses: None,
        }
    }

    impl SRow {
        /// items.id=889: the guard_clauses column, raw text (so a test can store
        /// anything).
        fn with_clauses(mut self, raw: &str) -> Self {
            self.clauses = Some(raw.to_owned());
            self
        }

        fn guarded(
            mut self,
            var: &'static str,
            op: &'static str,
            value: Option<&'static str>,
        ) -> Self {
            self.guard = Some((var, op, value));
            self
        }
    }

    async fn seed_store_focus(pool: &sqlx::SqlitePool, focus_id: &str, rows: Vec<SRow>) {
        let mut conn = pool.acquire().await.unwrap();
        sqlx::query("PRAGMA foreign_keys = OFF")
            .execute(&mut *conn)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO focuses
                (focus_id, display_name, description, version, max_routing_tier,
                 output_type, suggest_in_focuses, multi_source_validation,
                 generic_title_template, status, created_at, updated_at)
             VALUES (?, 'Store Writer Test Focus', '', '1.0', 'local_only',
                     'general', '[]', 0, 'Hidden item', 'shipped',
                     datetime('now'), datetime('now'))",
        )
        .bind(focus_id)
        .execute(&mut *conn)
        .await
        .unwrap();
        for (seq, row) in rows.iter().enumerate() {
            let comp_id = format!("comp-{focus_id}-{}", row.id);
            let (gv, go, gval) = match row.guard {
                Some((v, o, val)) => (Some(v), Some(o), val),
                None => (None, None, None),
            };
            let (anchor, offset) = if row.trigger {
                (Some("topic_date"), Some("+1d"))
            } else {
                (None, None)
            };
            sqlx::query(
                "INSERT INTO focus_block_compositions
                    (id, focus_id, action_id, block_stable_id, sequence_index,
                     schedule_trigger_anchor_field, schedule_trigger_offset,
                     guard_var, guard_op, guard_value, guard_clauses,
                     store_op, store_target, store_args, created_at, updated_at)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, datetime('now'), datetime('now'))",
            )
            .bind(&comp_id)
            .bind(focus_id)
            .bind(row.id)
            .bind(row.block)
            .bind(seq as i64)
            .bind(anchor)
            .bind(offset)
            .bind(gv)
            .bind(go)
            .bind(gval)
            .bind(row.clauses.as_deref())
            .bind(row.op)
            .bind(row.target)
            .bind(row.args)
            .execute(&mut *conn)
            .await
            .unwrap();
            sqlx::query(
                "INSERT INTO focus_block_customization
                    (id, composition_id, persona_id, customization, created_at, updated_at)
                 VALUES (?, ?, NULL, ?, datetime('now'), datetime('now'))",
            )
            .bind(format!("cust-{comp_id}"))
            .bind(&comp_id)
            .bind(&row.custom)
            .execute(&mut *conn)
            .await
            .unwrap();
        }
    }

    async fn store_load(focus_id: &str, rows: Vec<SRow>) -> Result<FocusDefinition, String> {
        let pool = block_load_pool().await;
        seed_store_focus(&pool, focus_id, rows).await;
        match load_focus_definition_from_db(&pool, focus_id).await {
            Ok(Some(def)) => Ok(def),
            Ok(None) => panic!("no focuses row"),
            Err(LifecycleError::ValidationFailed(m)) => Err(m),
            Err(other) => panic!("expected a LOAD ValidationFailed, got {other:?}"),
        }
    }

    const PUT_INTENT: &str = r#"{"value": "intent_out"}"#;

    #[tokio::test]
    async fn store_load_good_writer_rows_populate_the_typed_row_and_the_step_shape() {
        with_temp_data_root(async {
            let def = store_load(
                "sw-ok",
                vec![
                    smodel("route", "intent_out"),
                    swriter(
                        "lock",
                        "session_put",
                        Some("locked_intent"),
                        Some(PUT_INTENT),
                    ),
                ],
            )
            .await
            .unwrap();
            assert!(def.steps[0].store_op.is_none());
            let w = &def.steps[1];
            let op = w.store_op.as_ref().expect("a typed store row");
            assert_eq!(op.op, store_op::Op::SessionPut);
            assert_eq!(op.target.as_deref(), Some("locked_intent"));
            assert_eq!(op.arg(store_op::Slot::Value), Some("intent_out"));
            // Not a model step, not a block: LocalOnly, no template, no output.
            assert_eq!(w.routing_tier, ExternalAccess::LocalOnly);
            assert!(w.block_stable_id.is_none() && w.output_var.is_none());
            assert!(w.prompt_template.is_empty() && !w.requires_user_handoff);
            assert_eq!(analyze_guarded_composition("sw-ok", &def.steps).unwrap(), 1);
        })
        .await;
    }

    /// A whole case flow: ids published by one writer feed a later writer and a
    /// guard, every operation of the closed set loads.
    #[tokio::test]
    async fn store_load_a_full_case_flow_loads() {
        with_temp_data_root(async {
            let def = store_load(
                "sw-flow",
                vec![
                    smodel("ent", "ent_out"),
                    smodel("lab", "label_out"),
                    smodel("sym", "sym_out"),
                    smodel("strat", "strat_out"),
                    smodel("outc", "outc_out"),
                    smodel("ents", "ents_out"),
                    smodel("desc", "desc_out"),
                    smodel("hand", "hand_out"),
                    swriter("hold", "latch", None, None).guarded(
                        "write_mode",
                        "eq",
                        Some("suppressed"),
                    ),
                    swriter(
                        "open",
                        "case_open",
                        Some("new_case"),
                        Some(r#"{"entity":"ent_out","label":"label_out","symptoms":"sym_out"}"#),
                    )
                    .guarded("write_mode", "eq", Some("open")),
                    swriter(
                        "remember",
                        "session_put",
                        Some("active_case_id"),
                        Some(r#"{"value":"new_case"}"#),
                    )
                    .guarded("new_case", "present", None),
                    swriter(
                        "log",
                        "strategy_append",
                        None,
                        Some(r#"{"case":"new_case","strategy":"strat_out","outcome":"outc_out"}"#),
                    )
                    .guarded("new_case", "present", None),
                    swriter(
                        "advance",
                        "case_update",
                        None,
                        Some(r#"{"case":"active_case_id","outcome":"outc_out"}"#),
                    )
                    .guarded("active_case_id", "present", None),
                    swriter(
                        "delta",
                        "delta_record",
                        Some("new_delta"),
                        Some(
                            r#"{"entities":"ents_out","description":"desc_out","case":"new_case"}"#,
                        ),
                    )
                    .guarded("new_case", "present", None),
                    swriter(
                        "confirm",
                        "delta_confirm",
                        None,
                        Some(r#"{"handle":"hand_out"}"#),
                    )
                    .guarded("new_delta", "present", None),
                    swriter(
                        "event",
                        "delta_changed",
                        None,
                        Some(r#"{"delta":"new_delta","entity":"ent_out"}"#),
                    )
                    .guarded("new_delta", "present", None),
                    smodel("answer", "answer_out"),
                ],
            )
            .await;
            let def = def.unwrap_or_else(|m| panic!("a full case flow must load: {m}"));
            let ops: Vec<&str> = def
                .steps
                .iter()
                .filter_map(|s| s.store_op.as_ref().map(|o| o.op.as_str()))
                .collect();
            assert_eq!(
                ops,
                [
                    "latch",
                    "case_open",
                    "session_put",
                    "strategy_append",
                    "case_update",
                    "delta_record",
                    "delta_confirm",
                    "delta_changed"
                ]
            );
        })
        .await;
    }

    #[tokio::test]
    async fn store_load_rejections_each_have_their_own_message() {
        with_temp_data_root(async {
            let put = |target: &'static str| {
                vec![
                    smodel("route", "intent_out"),
                    swriter("w", "session_put", Some(target), Some(PUT_INTENT)),
                ]
            };
            let with_arg = |op: &'static str, target: Option<&'static str>, args: &'static str| {
                vec![
                    smodel("a", "v"),
                    swriter("w", op, target, Some(args)),
                    smodel("z", "zz"),
                ]
            };
            let cases: Vec<(&str, Vec<SRow>, &str)> = vec![
                (
                    "unknown op",
                    vec![smodel("a", "v"), swriter("w", "drop_table", None, None)],
                    "unknown store_op 'drop_table'",
                ),
                (
                    "target without op",
                    vec![
                        smodel("a", "v"),
                        SRow {
                            target: Some("locked_intent"),
                            ..swriter("w", "latch", None, None)
                        }
                        .no_op(),
                    ],
                    "without store_op",
                ),
                (
                    "latch through session_put",
                    put("write_mode"),
                    "can only be set by the latch operation",
                ),
                ("derived target", put("case_context"), "derived, read-only"),
                (
                    "unknown target",
                    put("anything_else"),
                    "not a registered session variable",
                ),
                (
                    "latch with args",
                    vec![smodel("a", "v"), swriter("w", "latch", None, Some("{}"))],
                    "takes no store_args",
                ),
                (
                    "unknown slot",
                    with_arg(
                        "session_put",
                        Some("locked_intent"),
                        r#"{"value":"v","bogus":"v"}"#,
                    ),
                    "has no slot 'bogus'",
                ),
                (
                    "missing slot",
                    with_arg("case_open", None, r#"{"entity":"v"}"#),
                    "needs slot 'label'",
                ),
                (
                    "argument from a later row",
                    vec![
                        swriter("w", "session_put", Some("locked_intent"), Some(PUT_INTENT)),
                        smodel("route", "intent_out"),
                    ],
                    "cannot sit before the data it needs",
                ),
                (
                    "argument nobody produces",
                    with_arg("session_put", Some("locked_intent"), r#"{"value":"ghost"}"#),
                    "no earlier step produces",
                ),
                (
                    "derived argument",
                    with_arg(
                        "session_put",
                        Some("locked_intent"),
                        r#"{"value":"case_context"}"#,
                    ),
                    "derived name",
                ),
                (
                    "system token argument",
                    with_arg(
                        "session_put",
                        Some("locked_intent"),
                        r#"{"value":"user_input"}"#,
                    ),
                    "never a store argument",
                ),
                (
                    "write_mode argument",
                    with_arg(
                        "session_put",
                        Some("locked_intent"),
                        r#"{"value":"write_mode"}"#,
                    ),
                    "only the latch operation touches the one-time latch",
                ),
                (
                    "published name collides",
                    with_arg(
                        "case_open",
                        Some("active_case_id"),
                        r#"{"entity":"v","label":"v"}"#,
                    ),
                    "collides",
                ),
                (
                    "optional session argument unguarded",
                    with_arg(
                        "case_update",
                        None,
                        r#"{"case":"active_case_id","outcome":"v"}"#,
                    ),
                    "needs a guard on 'active_case_id'",
                ),
                (
                    "writer cites a block",
                    vec![
                        smodel("a", "v"),
                        SRow {
                            block: Some("cb-06"),
                            ..swriter("w", "latch", None, None)
                        },
                    ],
                    "cannot also cite a building block",
                ),
                (
                    "writer with a schedule trigger",
                    vec![
                        smodel("a", "v"),
                        SRow {
                            trigger: true,
                            ..swriter("w", "latch", None, None)
                        },
                    ],
                    "cannot carry a schedule trigger",
                ),
                (
                    "writer with customization",
                    vec![
                        smodel("a", "v"),
                        SRow {
                            custom: r#"{"prompt_template":"x"}"#.to_owned(),
                            ..swriter("w", "latch", None, None)
                        },
                    ],
                    "takes no customization",
                ),
                (
                    "a path of writers alone",
                    vec![swriter("w", "latch", None, None)],
                    "runs no step",
                ),
                (
                    "guard on an unknown variable still checked",
                    vec![
                        smodel("a", "v"),
                        swriter("w", "latch", None, None).guarded("ghost", "present", None),
                    ],
                    "unknown variable",
                ),
            ];
            for (i, (label, rows, want)) in cases.into_iter().enumerate() {
                let m = store_load(&format!("sw-rej-{i}"), rows).await.unwrap_err();
                assert!(m.contains(want), "{label}: expected '{want}' in: {m}");
                assert!(m.contains(&format!("Focus 'sw-rej-{i}'")), "{label}: {m}");
            }
        })
        .await;
    }

    impl SRow {
        /// A row with store_target set and store_op NULL (half set).
        fn no_op(mut self) -> Self {
            self.op = None;
            self
        }
    }

    /// A writer's arguments may be tainted (the strategy log is a summary written
    /// by a step whose prompt held case_context). The writer is not tainted
    /// itself: its template is empty and it records nothing, so extraction and
    /// the external-step rules see exactly what they saw before.
    #[tokio::test]
    async fn store_load_a_tainted_argument_is_allowed_and_the_writer_is_not_tainted() {
        with_temp_data_root(async {
            let mut summarise = smodel("summarise", "summary");
            summarise.custom = serde_json::json!({
                "routing_tier": 1,
                "prompt_template": "Summarise: {case_context}",
                "output_var": "summary"
            })
            .to_string();
            let def = store_load(
                "sw-taint",
                vec![
                    summarise,
                    swriter(
                        "keep",
                        "session_put",
                        Some("locked_intent"),
                        Some(r#"{"value":"summary"}"#),
                    ),
                    smodel("answer", "answer_out"),
                ],
            )
            .await
            .unwrap_or_else(|m| panic!("a tainted argument must load: {m}"));
            let taint = crate::conductor::taint::taint_closure(
                &def.steps,
                &row_output_vars(&def.steps),
                crate::conductor::declared_inputs::bound_names(),
            );
            assert!(taint.actions.contains("summarise"));
            assert!(!taint.actions.contains("keep"), "the writer is not tainted");
            assert!(taint.vars.contains("summary"));
        })
        .await;
    }

    /// The path-sensitive half of "a write row cannot sit before the data it
    /// needs": the row that produces the argument is guarded away on some path
    /// that still runs the writer.
    #[tokio::test]
    async fn store_load_a_producer_that_does_not_run_before_the_writer_on_a_path_fails() {
        with_temp_data_root(async {
            let m = store_load(
                "sw-path",
                vec![
                    smodel("route", "r"),
                    smodel("produce", "v").guarded("r", "eq", Some("x")),
                    swriter(
                        "w",
                        "session_put",
                        Some("locked_intent"),
                        Some(r#"{"value":"v"}"#),
                    )
                    .guarded("r", "present", None),
                ],
            )
            .await
            .unwrap_err();
            assert!(
                m.contains("'w'")
                    && m.contains("reads 'v'")
                    && m.contains("does not run before it"),
                "{m}"
            );

            // Guarding the writer the same way as its producer loads.
            let ok = store_load(
                "sw-path-ok",
                vec![
                    smodel("route", "r"),
                    smodel("produce", "v").guarded("r", "eq", Some("x")),
                    swriter(
                        "w",
                        "session_put",
                        Some("locked_intent"),
                        Some(r#"{"value":"v"}"#),
                    )
                    .guarded("r", "eq", Some("x")),
                    smodel("fallback", "fb").guarded("r", "ne", Some("x")),
                ],
            )
            .await;
            assert!(ok.is_ok(), "{ok:?}");
        })
        .await;
    }

    /// A writer between a block and the next ordinary step is transparent to the
    /// block-placement rule: the block still needs an ordinary local step after.
    #[tokio::test]
    async fn store_load_writers_are_transparent_to_block_placement() {
        with_temp_data_root(async {
            let block = |id: &'static str| SRow {
                id,
                op: None,
                target: None,
                args: None,
                guard: None,
                block: Some("cb-06"),
                custom: serde_json::json!({
                    "input_var": "draft_output",
                    "output_var": "verdict",
                    "criteria": [{"name": "has_content", "kind": "non_empty"}],
                    "on_fail": "advisory"
                })
                .to_string(),
                trigger: false,
                clauses: None,
            };
            // block, writer, END: the block's verdict would become the output.
            let m = store_load(
                "sw-blk-last",
                vec![
                    smodel("draft_writing", "draft_output"),
                    block("check"),
                    swriter("w", "latch", None, None),
                ],
            )
            .await
            .unwrap_err();
            assert!(m.contains("cannot be the final step"), "{m}");
            // block, writer, ordinary step: fine.
            assert!(store_load(
                "sw-blk-ok",
                vec![
                    smodel("draft_writing", "draft_output"),
                    block("check"),
                    swriter("w", "latch", None, None),
                    smodel("tail", "tail_out"),
                ],
            )
            .await
            .is_ok());
        })
        .await;
    }

    /// Store writers are database-only (decisions.id=938): a .focus file cannot
    /// express one, and a store_op key in a YAML step has no effect.
    #[test]
    fn a_yaml_step_cannot_be_a_store_writer() {
        let def = parse_focus_text(
            "id: yaml-focus\nsteps:\n  a:\n    store_op: latch\n    store_target: locked_intent\n    prompt_template: hi\n",
        )
        .unwrap();
        assert_eq!(def.steps.len(), 1);
        assert!(def.steps[0].store_op.is_none());
        assert_eq!(def.steps[0].prompt_template, "hi");
    }

    /// Gate3's look-ahead sees through a writer: it makes no call and ends no scan.
    #[test]
    fn the_look_ahead_skips_store_writers() {
        let model = |tier: ExternalAccess| {
            let mut s = hand_built_cb06_step();
            s.block_stable_id = None;
            s.block_customization = None;
            s.routing_tier = tier;
            s
        };
        let writer = || {
            store_writer_step(
                "f",
                "w",
                false,
                false,
                "{}",
                "g",
                store_op::StoreOp::from_columns(Some("latch"), None, None)
                    .unwrap()
                    .unwrap(),
            )
            .unwrap()
        };
        let steps = [
            model(ExternalAccess::LocalOnly),
            writer(),
            model(ExternalAccess::AnonymousRequired),
        ];
        let ceiling = ExternalAccess::AnonymousRequired;
        // Without the skip the unguarded writer (LocalOnly) would end the scan
        // and hide the external step behind it.
        assert_eq!(
            next_execution_tier(&steps, 0, ceiling),
            Some(execution_tier_ordinal(ExternalAccess::AnonymousRequired))
        );
    }

    // -- items.id=889: AND-guards, handles, the new operations, the lint ------

    fn clause_json(var: &str, op: &str, value: Option<serde_json::Value>) -> serde_json::Value {
        let mut o = serde_json::Map::new();
        o.insert("var".into(), var.into());
        o.insert("op".into(), op.into());
        if let Some(v) = value {
            o.insert("value".into(), v);
        }
        serde_json::Value::Object(o)
    }

    /// Two model rows then a row with a single guard AND a clause list.
    fn and_rows(extra: Vec<SRow>) -> Vec<SRow> {
        let mut rows = vec![smodel("route", "intent"), smodel("step", "stage")];
        rows.extend(extra);
        rows
    }

    #[tokio::test]
    async fn and_guard_load_accepts_a_single_guard_plus_a_clause_list_and_keeps_both() {
        with_temp_data_root(async {
            let clauses = serde_json::json!([
                clause_json("stage", "in", Some(serde_json::json!(["one", "two"]))),
                clause_json("write_mode", "eq", Some("open".into())),
            ])
            .to_string();
            let def = store_load(
                "and-ok",
                and_rows(vec![
                    smodel("both", "out")
                        .guarded("intent", "eq", Some("a"))
                        .with_clauses(&clauses),
                    // The legacy single guard alone is unchanged.
                    smodel("legacy", "out2").guarded("intent", "ne", Some("a")),
                    // A clause list alone, no single guard.
                    smodel("list_only", "out3").with_clauses(
                        &serde_json::json!([clause_json("intent", "present", None)]).to_string(),
                    ),
                    // `stage` is a router (the in clause), so it needs a row
                    // that still runs when it matches none of its literals.
                    smodel("stage_fb", "out4").guarded("stage", "present", None),
                ]),
            )
            .await;
            let def = def.unwrap_or_else(|m| panic!("AND-guards must load: {m}"));
            let both = &def.steps[2];
            assert!(both.guard.is_some());
            assert_eq!(both.and_guards.len(), 2);
            assert_eq!(both.guards().len(), 3);
            assert!(def.steps[3].and_guards.is_empty() && def.steps[3].guard.is_some());
            assert!(def.steps[4].guard.is_none() && def.steps[4].and_guards.len() == 1);
            // Every other row has no clause list.
            assert!(def.steps[..2].iter().all(|s| !s.is_guarded()));
        })
        .await;
    }

    #[tokio::test]
    async fn and_guard_load_rejections_each_have_their_own_message() {
        with_temp_data_root(async {
            let cases: Vec<(&str, String, &str)> = vec![
                (
                    "not-json",
                    "not json".to_owned(),
                    "not a JSON array of clause objects",
                ),
                (
                    "not-array",
                    r#"{"var":"intent"}"#.to_owned(),
                    "not a JSON array of clause objects",
                ),
                ("empty", "[]".to_owned(), "empty array"),
                (
                    "unknown-key",
                    r#"[{"var":"intent","op":"present","extra":1}]"#.to_owned(),
                    "unknown field",
                ),
                ("no-op", r#"[{"var":"intent"}]"#.to_owned(), "missing field"),
                (
                    "not-object",
                    r#"["intent"]"#.to_owned(),
                    "not a JSON array of clause objects",
                ),
                (
                    "bad-operator",
                    r#"[{"var":"intent","op":"matches","value":"x"}]"#.to_owned(),
                    "unknown guard operator 'matches'",
                ),
                (
                    "eq-array",
                    r#"[{"var":"intent","op":"eq","value":["a"]}]"#.to_owned(),
                    "one JSON string",
                ),
                (
                    "in-string",
                    r#"[{"var":"intent","op":"in","value":"a"}]"#.to_owned(),
                    "JSON array of strings",
                ),
                (
                    "eq-no-value",
                    r#"[{"var":"intent","op":"eq"}]"#.to_owned(),
                    "needs a guard_value",
                ),
                (
                    "present-with-value",
                    r#"[{"var":"intent","op":"present","value":"x"}]"#.to_owned(),
                    "takes no guard_value",
                ),
                (
                    "in-empty",
                    r#"[{"var":"intent","op":"in","value":[]}]"#.to_owned(),
                    "the array is empty",
                ),
                (
                    "duplicate",
                    r#"[{"var":"intent","op":"present"},{"var":"intent","op":"present"}]"#
                        .to_owned(),
                    "duplicates an earlier clause",
                ),
                (
                    "empty-var",
                    r#"[{"var":" ","op":"present"}]"#.to_owned(),
                    "variable name is empty",
                ),
            ];
            for (label, raw, expect) in cases {
                let m = store_load(
                    &format!("and-bad-{label}"),
                    and_rows(vec![smodel("bad", "out").with_clauses(&raw)]),
                )
                .await
                .unwrap_err();
                assert!(m.contains("step 'bad'"), "{label}: {m}");
                assert!(
                    m.contains("guard_clauses[") || m.contains("guard_clauses"),
                    "{label}: {m}"
                );
                assert!(m.contains(expect), "{label}: expected '{expect}' in: {m}");
            }
        })
        .await;
    }

    #[tokio::test]
    async fn and_guard_load_caps_the_whole_clause_set_single_guard_included() {
        with_temp_data_root(async {
            let four = serde_json::json!([
                clause_json("intent", "present", None),
                clause_json("stage", "present", None),
                clause_json("third", "present", None),
                clause_json("stage", "ne", Some("y".into())),
            ])
            .to_string();
            // Four clauses in the list alone is exactly the cap.
            let def = store_load(
                "and-cap-ok",
                and_rows(vec![
                    smodel("third_step", "third"),
                    smodel("four", "out").with_clauses(&four),
                ]),
            )
            .await;
            assert!(def.is_ok(), "{:?}", def.err());
            // One single guard plus four in the list is five.
            let m = store_load(
                "and-cap-over",
                and_rows(vec![
                    smodel("third_step", "third"),
                    smodel("five", "out")
                        .guarded("intent", "eq", Some("a"))
                        .with_clauses(&four),
                ]),
            )
            .await
            .unwrap_err();
            assert!(m.contains("at most 4 guard clauses in total"), "{m}");
            assert!(m.contains("this one has 5"), "{m}");
            // The single guard repeated in the list is a duplicate.
            let dup =
                serde_json::json!([clause_json("intent", "eq", Some("a".into()))]).to_string();
            let m = store_load(
                "and-cap-dup",
                and_rows(vec![smodel("dup", "out")
                    .guarded("intent", "eq", Some("a"))
                    .with_clauses(&dup)]),
            )
            .await
            .unwrap_err();
            assert!(m.contains("duplicates an earlier clause"), "{m}");
        })
        .await;
    }

    #[tokio::test]
    async fn and_guard_load_checks_every_clause_variable_and_the_clause_sets_paths() {
        with_temp_data_root(async {
            // An unknown variable in the SECOND clause.
            let unk = serde_json::json!([
                clause_json("intent", "present", None),
                clause_json("nope", "present", None),
            ])
            .to_string();
            let m = store_load(
                "and-unk",
                and_rows(vec![smodel("u", "o").with_clauses(&unk)]),
            )
            .await
            .unwrap_err();
            assert!(
                m.contains("step 'u'") && m.contains("'nope'") && m.contains("unknown variable"),
                "{m}"
            );

            // Two different eq literals on one variable never hold together.
            let contra =
                serde_json::json!([clause_json("intent", "eq", Some("b".into()))]).to_string();
            let m = store_load(
                "and-contra",
                and_rows(vec![
                    smodel("a", "o").guarded("intent", "eq", Some("a")),
                    smodel("never", "o2")
                        .guarded("intent", "eq", Some("a"))
                        .with_clauses(&contra),
                    smodel("fb", "o3").guarded("intent", "ne", Some("a")),
                    smodel("fb2", "o4").guarded("intent", "present", None),
                ]),
            )
            .await
            .unwrap_err();
            assert!(m.contains("step 'never' is unreachable"), "{m}");

            // The router fallback rule over clause sets: a row with an ne clause
            // AND another clause still counts, because on OTHER it runs only when
            // both hold. Here the second clause never blocks it.
            let ne_and = serde_json::json!([clause_json("stage", "present", None)]).to_string();
            let def = store_load(
                "and-fallback",
                and_rows(vec![
                    smodel("a", "o").guarded("intent", "eq", Some("a")),
                    smodel("fb", "o2")
                        .guarded("intent", "ne", Some("a"))
                        .with_clauses(&ne_and),
                ]),
            )
            .await;
            assert!(def.is_ok(), "{:?}", def.err());
            // Without any row that can run on OTHER the router has no fallback.
            let m = store_load(
                "and-nofallback",
                and_rows(vec![
                    smodel("a", "o").guarded("intent", "eq", Some("a")),
                    smodel("b", "o2")
                        .guarded("intent", "eq", Some("b"))
                        .with_clauses(&ne_and),
                    smodel("end", "o3"),
                ]),
            )
            .await
            .unwrap_err();
            assert!(
                m.contains("router variable 'intent' has no fallback row"),
                "{m}"
            );
        })
        .await;
    }

    /// The optional session-bound argument rule (items.id=887) no longer needs
    /// the argument to be the row's ONLY guard.
    #[tokio::test]
    async fn an_optional_session_argument_may_be_covered_by_any_clause_of_the_row() {
        with_temp_data_root(async {
            let args = Some(r#"{"case":"active_case_id","outcome":"intent"}"#);
            let covered =
                serde_json::json!([clause_json("active_case_id", "present", None)]).to_string();
            // The single guard is another variable, the clause list covers it.
            let def = store_load(
                "and-opt-ok",
                vec![
                    smodel("route", "intent"),
                    swriter("up", "case_update", None, args)
                        .guarded("intent", "eq", Some("close"))
                        .with_clauses(&covered),
                    smodel("fb", "o").guarded("intent", "ne", Some("close")),
                ],
            )
            .await;
            assert!(def.is_ok(), "{:?}", def.err());
            // Not covered by any clause: refused, as before.
            let other = serde_json::json!([clause_json("intent", "present", None)]).to_string();
            let m = store_load(
                "and-opt-bad",
                vec![
                    smodel("route", "intent"),
                    swriter("up", "case_update", None, args)
                        .guarded("intent", "eq", Some("close"))
                        .with_clauses(&other),
                    smodel("fb", "o").guarded("intent", "ne", Some("close")),
                ],
            )
            .await
            .unwrap_err();
            assert!(
                m.contains("can be absent") && m.contains("active_case_id"),
                "{m}"
            );
            // An `absent` clause on the argument does not cover it.
            let absent =
                serde_json::json!([clause_json("active_case_id", "absent", None)]).to_string();
            let m = store_load(
                "and-opt-absent",
                vec![
                    smodel("route", "intent"),
                    swriter("up", "case_update", None, args)
                        .guarded("intent", "eq", Some("close"))
                        .with_clauses(&absent),
                    smodel("fb", "o").guarded("intent", "ne", Some("close")),
                ],
            )
            .await
            .unwrap_err();
            assert!(m.contains("can be absent"), "{m}");
            // The legacy shape, the argument as the single guard, still loads.
            let def = store_load(
                "and-opt-legacy",
                vec![
                    smodel("route", "intent"),
                    swriter("up", "case_update", None, args).guarded(
                        "active_case_id",
                        "present",
                        None,
                    ),
                ],
            )
            .await;
            assert!(def.is_ok(), "{:?}", def.err());
        })
        .await;
    }

    #[tokio::test]
    async fn a_handle_slot_takes_model_output_never_a_bound_name_or_a_published_id() {
        with_temp_data_root(async {
            // Model output: fine, for both handle operations.
            let def = store_load(
                "hnd-ok",
                vec![
                    smodel("pick", "ticket_handle"),
                    swriter(
                        "sel",
                        "case_select",
                        None,
                        Some(r#"{"handle":"ticket_handle"}"#),
                    ),
                    smodel("pick2", "delta_handle"),
                    swriter(
                        "conf",
                        "delta_confirm",
                        None,
                        Some(r#"{"handle":"delta_handle"}"#),
                    ),
                ],
            )
            .await;
            assert!(def.is_ok(), "{:?}", def.err());
            // A seeded name is an internal id or an intent, not a handle.
            for name in ["active_case_id", "locked_intent"] {
                let args = format!(r#"{{"handle":"{name}"}}"#);
                let args: &'static str = Box::leak(args.into_boxed_str());
                let m = store_load(
                    &format!("hnd-bound-{name}"),
                    vec![
                        smodel("route", "intent"),
                        swriter("sel", "case_select", None, Some(args))
                            .guarded(name, "present", None),
                        smodel("fb", "o").guarded("intent", "present", None),
                    ],
                )
                .await
                .unwrap_err();
                assert!(m.contains("a bound name") && m.contains("handle"), "{m}");
            }
            // The id a case_open published is an internal id.
            let m = store_load(
                "hnd-published",
                vec![
                    smodel("ent", "ent_out"),
                    smodel("lab", "label_out"),
                    swriter(
                        "open",
                        "case_open",
                        Some("new_case"),
                        Some(r#"{"entity":"ent_out","label":"label_out"}"#),
                    ),
                    swriter("sel", "case_select", None, Some(r#"{"handle":"new_case"}"#)),
                    smodel("tail", "t"),
                ],
            )
            .await
            .unwrap_err();
            assert!(m.contains("an id published by a store writer"), "{m}");
        })
        .await;
    }

    // -- the close-ticket lint ----------------------------------------------

    /// Rows for the lint tests: a closure router, a strategy log and a closing
    /// update, in the order given.
    fn close_rows(order: &[&'static str], pin: Option<&'static str>) -> Vec<SRow> {
        let ac = |r: SRow| r.guarded("active_case_id", "present", None);
        let mut rows = vec![smodel("route", "closure"), smodel("note", "note")];
        for id in order {
            let row = match *id {
                "log" => ac(swriter(
                    "log",
                    "strategy_append",
                    None,
                    Some(r#"{"case":"active_case_id","strategy":"note","outcome":"note"}"#),
                )),
                "close" => ac(swriter(
                    "close",
                    "case_update",
                    None,
                    Some(r#"{"case":"active_case_id","state":"closure"}"#),
                )),
                "link" => ac(swriter(
                    "link",
                    "case_link",
                    None,
                    Some(r#"{"case":"active_case_id","entity":"note"}"#),
                )),
                other => panic!("{other}"),
            };
            let row = match (*id, pin) {
                ("close", Some(literals)) => row.with_clauses(
                    &serde_json::json!([clause_json(
                        "closure",
                        "in",
                        Some(serde_json::from_str(literals).unwrap())
                    )])
                    .to_string(),
                ),
                _ => row,
            };
            rows.push(row);
        }
        // A reply and a fallback for the router.
        rows.push(smodel("reply", "reply").guarded("closure", "present", None));
        rows
    }

    #[tokio::test]
    async fn the_closing_case_update_must_be_the_last_case_write_on_its_path() {
        with_temp_data_root(async {
            let terminal = Some(r#"["resolved","replaced","closed"]"#);
            // Log first, close last: loads.
            let ok = store_load("cl-ok", close_rows(&["log", "close"], terminal)).await;
            assert!(ok.is_ok(), "{:?}", ok.err());
            // Close first, then a strategy append to the closed case: refused.
            let m = store_load("cl-bad", close_rows(&["close", "log"], terminal))
                .await
                .unwrap_err();
            assert!(
                m.contains(
                    "step 'close' may close the ticket but step 'log' writes the case after it"
                ),
                "{m}"
            );
            assert!(m.contains("closure='resolved'"), "the path is named: {m}");
            // An entity link after the close is a case write too.
            let m = store_load("cl-link", close_rows(&["close", "link"], terminal))
                .await
                .unwrap_err();
            assert!(m.contains("step 'link' writes the case after it"), "{m}");
        })
        .await;
    }

    #[tokio::test]
    async fn a_state_pinned_to_non_terminal_literals_may_be_followed_by_case_writes() {
        with_temp_data_root(async {
            let open_states = Some(r#"["open","in-progress","escalated","awaiting-external"]"#);
            let ok = store_load("cl-pin-open", close_rows(&["close", "log"], open_states)).await;
            assert!(ok.is_ok(), "{:?}", ok.err());
            // Pinned, but one literal is terminal: that path closes.
            let mixed = Some(r#"["in-progress","closed"]"#);
            let m = store_load("cl-pin-mixed", close_rows(&["close", "log"], mixed))
                .await
                .unwrap_err();
            assert!(m.contains("closure='closed'"), "{m}");
        })
        .await;
    }

    #[tokio::test]
    async fn an_unpinned_state_is_treated_as_possibly_closing() {
        with_temp_data_root(async {
            // No clause names the state: the cautious side.
            let m = store_load("cl-unpinned", close_rows(&["close", "log"], None))
                .await
                .unwrap_err();
            assert!(m.contains("may close the ticket"), "{m}");
            // The same rows in the safe order load.
            let ok = store_load("cl-unpinned-ok", close_rows(&["log", "close"], None)).await;
            assert!(ok.is_ok(), "{:?}", ok.err());
        })
        .await;
    }

    /// A case_update that sets no state cannot close, so a later write is fine.
    #[tokio::test]
    async fn a_case_update_that_sets_no_state_is_never_a_closing_write() {
        with_temp_data_root(async {
            let ok = store_load(
                "cl-nostate",
                vec![
                    smodel("route", "intent"),
                    smodel("note", "note"),
                    swriter(
                        "outcome",
                        "case_update",
                        None,
                        Some(r#"{"case":"active_case_id","outcome":"note"}"#),
                    )
                    .guarded("active_case_id", "present", None),
                    swriter(
                        "log",
                        "strategy_append",
                        None,
                        Some(r#"{"case":"active_case_id","strategy":"note","outcome":"note"}"#),
                    )
                    .guarded("active_case_id", "present", None),
                    smodel("reply", "reply"),
                ],
            )
            .await;
            assert!(ok.is_ok(), "{:?}", ok.err());
        })
        .await;
    }

    // -- ACCEPTANCE: the six-intent Tech Support draft ------------------------

    fn draft_srows(variant: crate::conductor::tech_support_draft::Variant) -> Vec<SRow> {
        use crate::conductor::tech_support_draft::{rows, Clause, Kind};
        fn json(rest: &[Clause]) -> Option<String> {
            if rest.is_empty() {
                return None;
            }
            Some(
                serde_json::Value::Array(
                    rest.iter()
                        .map(|(var, op, value)| {
                            let v = value.map(|v| {
                                if *op == "in" {
                                    serde_json::from_str(v).expect("an in list is JSON")
                                } else {
                                    serde_json::Value::String(v.to_owned())
                                }
                            });
                            clause_json(var, op, v)
                        })
                        .collect(),
                )
                .to_string(),
            )
        }
        rows(variant)
            .into_iter()
            .map(|r| {
                let mut row = match &r.kind {
                    Kind::Model { template, out } => {
                        let mut s = smodel(r.id, out);
                        s.custom = serde_json::json!({
                            "routing_tier": 1, "prompt_template": template, "output_var": out
                        })
                        .to_string();
                        s
                    }
                    Kind::Writer { op, target, args } => swriter(r.id, op, *target, *args),
                };
                if let Some((v, o, val)) = r.clauses.first().copied() {
                    row.guard = Some((v, o, val));
                }
                if r.clauses.len() > 1 {
                    row.clauses = json(&r.clauses[1..]);
                }
                row
            })
            .collect()
    }

    /// What the report says about one loaded draft.
    struct DraftReport {
        rows: usize,
        paths: usize,
        calls_min: usize,
        calls_max: usize,
        wasted_total: usize,
        wasted_max: usize,
        wasted_paths: usize,
        by_intent: Vec<(String, usize, usize, usize)>,
    }

    fn report_draft(def: &FocusDefinition) -> (DraftReport, Vec<guard::Path>) {
        let paths = guarded_composition_paths("draft", &def.steps).expect("the draft analyses");
        let is_model = |i: usize| def.steps[i].store_op.is_none();
        let mut calls_min = usize::MAX;
        let mut calls_max = 0;
        let (mut wasted_total, mut wasted_max, mut wasted_paths) = (0, 0, 0);
        let mut by_intent: std::collections::BTreeMap<String, (usize, usize, usize)> =
            Default::default();
        for p in &paths {
            let models: Vec<usize> = p.rows.iter().copied().filter(|&i| is_model(i)).collect();
            calls_min = calls_min.min(models.len());
            calls_max = calls_max.max(models.len());
            // A call is WASTED when it is not the reply (the last model row) and
            // nothing that runs after it on the path reads its variable: not a
            // clause, not a writer argument, not a template token.
            let last = models.last().copied();
            let mut wasted = 0;
            for (pos, &i) in p.rows.iter().enumerate() {
                if !is_model(i) || Some(i) == last {
                    continue;
                }
                let Some(var) = def.steps[i].output_var.as_deref() else {
                    continue;
                };
                let read = p.rows[pos + 1..].iter().any(|&j| {
                    let t = &def.steps[j];
                    t.guards().iter().any(|g| g.var == var)
                        || t.store_op
                            .as_ref()
                            .is_some_and(|o| o.arg_vars().any(|a| a == var))
                        || crate::conductor::executor::find_tokens(&t.prompt_template)
                            .iter()
                            .any(|tok| tok == var)
                });
                if !read {
                    wasted += 1;
                }
            }
            wasted_total += wasted;
            wasted_max = wasted_max.max(wasted);
            wasted_paths += usize::from(wasted > 0);
            let intent = p
                .values
                .iter()
                .find(|(v, _)| v == "intent")
                .and_then(|(_, v)| v.clone())
                .unwrap_or_else(|| "(unset)".to_owned());
            let e = by_intent
                .entry(intent.replace('\u{0}', "OTHER"))
                .or_insert((0, usize::MAX, 0));
            e.0 += 1;
            e.1 = e.1.min(models.len());
            e.2 = e.2.max(models.len());
        }
        (
            DraftReport {
                rows: def.steps.len(),
                paths: paths.len(),
                calls_min,
                calls_max,
                wasted_total,
                wasted_max,
                wasted_paths,
                by_intent: by_intent
                    .into_iter()
                    .map(|(k, (n, lo, hi))| (k, n, lo, hi))
                    .collect(),
            },
            paths,
        )
    }

    fn ran(def: &FocusDefinition, p: &guard::Path) -> Vec<String> {
        p.rows
            .iter()
            .map(|&i| def.steps[i].action_id.clone())
            .collect()
    }

    fn value<'a>(p: &'a guard::Path, var: &str) -> Option<&'a str> {
        p.values
            .iter()
            .find(|(v, _)| v == var)
            .and_then(|(_, v)| v.as_deref())
    }

    #[tokio::test]
    async fn the_six_intent_tech_support_draft_loads_and_enumerates_under_the_cap() {
        use crate::conductor::tech_support_draft::Variant;
        with_temp_data_root(async {
            let lean = store_load("draft-lean", draft_srows(Variant::Lean))
                .await
                .unwrap_or_else(|m| panic!("the six-intent draft must load: {m}"));
            let (r, paths) = report_draft(&lean);
            let naive = store_load("draft-naive", draft_srows(Variant::Naive))
                .await
                .unwrap_or_else(|m| panic!("the naive variant must load: {m}"));
            let (n, _) = report_draft(&naive);

            println!("== items.id=889 acceptance: six-intent Tech Support draft (test fixture) ==");
            for (label, rep) in [("lean", &r), ("naive", &n)] {
                println!(
                    "{label}: rows={} paths={} (cap {}) local calls per path {}..{} \
                     wasted local calls: total={} max/path={} paths-with-waste={}",
                    rep.rows,
                    rep.paths,
                    guard::MAX_PATHS,
                    rep.calls_min,
                    rep.calls_max,
                    rep.wasted_total,
                    rep.wasted_max,
                    rep.wasted_paths
                );
            }
            println!("lean, by classified intent: (intent, paths, min calls, max calls)");
            for (intent, count, lo, hi) in &r.by_intent {
                println!("  {intent:<18} paths={count:<3} calls={lo}..{hi}");
            }

            // Under the cap, with room.
            assert!(r.paths <= guard::MAX_PATHS, "paths {}", r.paths);
            assert!(r.rows >= 30, "a real six-intent shape: {} rows", r.rows);
            // The disambiguation pass is cheap and the new-issue pass is the
            // costly one, about six calls (decisions.id=940 ruling 7).
            assert_eq!(r.calls_min, 2);
            assert_eq!(r.calls_max, 6);
            // Lean wastes no local call anywhere, Naive does: the metric bites.
            assert_eq!(r.wasted_total, 0, "lean wastes nothing");
            assert!(n.wasted_total > 0, "naive: {}", n.wasted_total);
            assert!(n.wasted_paths > 0);

            // -- every intent is a path of its own, with its own reply --------
            let reply_for = [
                ("new_issue", "nc_reply"),
                ("continue_ticket", "resume_reply"),
                ("check_in_update", "ci_reply"),
                ("close_ticket", "close_reply"),
                ("device_library", "dl_reply"),
                ("one_time_session", "ot_reply"),
            ];
            for (intent, reply) in reply_for {
                assert!(
                    paths.iter().any(|p| value(p, "intent") == Some(intent)
                        && ran(&lean, p).contains(&reply.to_owned())),
                    "{intent} reaches {reply}"
                );
            }

            // -- decisions.id=515: Cases A, B and C ----------------------------
            let unsure = |state: &str| -> Vec<&guard::Path> {
                paths
                    .iter()
                    .filter(|p| {
                        value(p, "intent") == Some("unsure")
                            && value(p, "open_ticket_state") == Some(state)
                    })
                    .collect()
            };
            // Case A: ONE open ticket, intent unclear: surface it before anything
            // else. One extra local call, no case work, no new-issue extraction.
            let a = unsure("one");
            assert!(!a.is_empty());
            for p in &a {
                let ids = ran(&lean, p);
                assert!(ids.contains(&"ask_one_ticket".to_owned()), "{ids:?}");
                assert!(!ids.contains(&"ask_which_ticket".to_owned()));
                assert!(
                    !ids.iter().any(|i| i.starts_with("nc_") || i == "open_case"),
                    "{ids:?}"
                );
            }
            // Case B: MANY open tickets: the capped list.
            let b = unsure("many");
            assert!(!b.is_empty());
            for p in &b {
                let ids = ran(&lean, p);
                assert!(ids.contains(&"ask_which_ticket".to_owned()), "{ids:?}");
                assert!(!ids.contains(&"ask_one_ticket".to_owned()));
                assert!(
                    !ids.iter().any(|i| i.starts_with("nc_") || i == "open_case"),
                    "{ids:?}"
                );
            }
            // Case C: NO open ticket, intent unclear: straight into the new-issue
            // flow, no question. A saving chat opens the case.
            let c = unsure("none");
            assert!(!c.is_empty());
            for p in &c {
                let ids = ran(&lean, p);
                assert!(ids.contains(&"nc_reply".to_owned()), "{ids:?}");
                assert!(!ids.contains(&"ask_one_ticket".to_owned()));
                assert!(!ids.contains(&"ask_which_ticket".to_owned()));
                assert_eq!(
                    ids.contains(&"open_case".to_owned()),
                    value(p, "write_mode") == Some("open"),
                    "{ids:?}"
                );
            }
            // A new issue while tickets are open surfaces them first.
            assert!(paths.iter().any(|p| {
                value(p, "intent") == Some("new_issue")
                    && value(p, "open_ticket_state") == Some("one")
                    && ran(&lean, p).contains(&"confirm_new".to_owned())
            }));

            // -- the one-time session never saves ------------------------------
            for p in paths
                .iter()
                .filter(|p| value(p, "intent") == Some("one_time_session"))
            {
                let ids = ran(&lean, p);
                assert!(
                    ids.contains(&"one_time".to_owned()) && ids.contains(&"ot_reply".to_owned())
                );
                assert!(!ids.contains(&"lock_intent".to_owned()));
            }
            // A latched or saving chat decides every other writer: no path with
            // write_mode suppressed runs a case or delta writer except the latch.
            let non_latch_writers = |ids: &[String]| {
                ids.iter().any(|i| {
                    [
                        "open_case",
                        "select_new",
                        "link_affected",
                        "select_ticket",
                        "ci_changed",
                        "ci_confirm",
                        "close_log",
                        "close_write",
                        "close_clear",
                        "lock_intent",
                    ]
                    .contains(&i.as_str())
                })
            };
            for p in paths
                .iter()
                .filter(|p| value(p, "write_mode") == Some("suppressed"))
            {
                let ids = ran(&lean, p);
                assert!(!non_latch_writers(&ids), "{ids:?}");
            }
            // The close path ends its case writes with the closing update.
            for p in paths
                .iter()
                .filter(|p| ran(&lean, p).contains(&"close_write".to_owned()))
            {
                let ids = ran(&lean, p);
                let pos = |n: &str| ids.iter().position(|i| i == n).unwrap();
                assert!(pos("close_log") < pos("close_write"), "{ids:?}");
            }
            // The reply is the last model row on every path.
            for p in &paths {
                let last_model = p
                    .rows
                    .iter()
                    .rev()
                    .copied()
                    .find(|&i| lean.steps[i].store_op.is_none())
                    .unwrap();
                assert_eq!(
                    lean.steps[last_model].output_var.as_deref(),
                    Some("reply"),
                    "{:?}",
                    ran(&lean, p)
                );
            }
        })
        .await;
    }

    /// The draft is a fixture: nothing seeds it and no shipped Focus is wired to
    /// it. The shipped database carries no Tech Support composition.
    #[tokio::test]
    async fn no_tech_support_focus_is_seeded_by_the_draft() {
        with_temp_data_root(async {
            let pool = block_load_pool().await;
            let n: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM focus_block_compositions WHERE focus_id LIKE 'tech%'",
            )
            .fetch_one(&pool)
            .await
            .unwrap();
            assert_eq!(n, 0);
            let rows: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM focus_block_compositions WHERE guard_clauses IS NOT NULL",
            )
            .fetch_one(&pool)
            .await
            .unwrap();
            assert_eq!(rows, 0, "no shipped row carries a clause list");
        })
        .await;
    }

    // -- execute(): store writers over real encrypted outputs.db -------------

    fn writer_step(
        id: &str,
        op: &str,
        target: Option<&str>,
        args: serde_json::Value,
        guard: Option<(&str, &str, Option<&str>)>,
    ) -> ActionDefinition {
        let args = if args.is_null() {
            None
        } else {
            Some(args.to_string())
        };
        let parsed = store_op::StoreOp::from_columns(Some(op), target, args.as_deref())
            .expect("row parses")
            .expect("a writer");
        let mut s = store_writer_step("f", id, false, false, "{}", "g", parsed).unwrap();
        s.guard = guard
            .and_then(|(v, o, val)| guard::Guard::from_columns(Some(v), Some(o), val).unwrap());
        s
    }

    fn writer_run(
        steps: Vec<ActionDefinition>,
        chat: Option<&str>,
        recorded_vars: &[(&str, &str)],
    ) -> FocusRun<crate::conductor::privacy::logger::TestLogger> {
        let mut run = tier3_ready_run(false);
        let mut def = single_tier3_step_focus_def();
        def.steps = steps;
        run.focus_def = Some(def);
        run.context_key = chat.map(str::to_owned);
        for (i, (var, value)) in recorded_vars.iter().enumerate() {
            run.task_track.as_mut().unwrap().add_step(recorded(
                &format!("seed{i}"),
                Some(var),
                value,
            ));
        }
        run
    }

    async fn outputs(
        run: &FocusRun<crate::conductor::privacy::logger::TestLogger>,
    ) -> sqlx::SqliteConnection {
        open_outputs_db(&run.user_id, &run.persona_id, CRISIS_TEST_KEY_HEX)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn execute_runs_a_store_writer_and_it_records_emits_and_ledgers_nothing() {
        with_temp_data_root(async {
            let mut run = writer_run(
                vec![writer_step(
                    "lock",
                    "session_put",
                    Some("locked_intent"),
                    serde_json::json!({"value": "intent_out"}),
                    None,
                )],
                Some("chat-sw"),
                &[("intent_out", "billing")],
            );
            assert!(run.execute().await.unwrap().is_none());
            let mut conn = outputs(&run).await;
            let row = crate::persistence::session_store::get_session_variable(
                &mut conn,
                "chat-sw",
                "f",
                "locked_intent",
            )
            .await
            .unwrap()
            .expect("the writer wrote through the typed store");
            assert_eq!(row.value, "billing");
            assert_eq!(row.written_by_run, run.focus_run_id.clone().unwrap());
            // Only the seeded value is on the track: the writer recorded nothing.
            assert_eq!(ran_ids(&run), ["seed0"]);
            assert!(run.emitted_for_test.lock().unwrap().is_empty());
            let ledger: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM focus_run_steps")
                .fetch_one(&mut conn)
                .await
                .unwrap();
            assert_eq!(ledger, 0, "no ledger row for a writer");
        })
        .await;
    }

    /// Guards still apply to a writer: a row whose guard is false writes nothing.
    #[tokio::test]
    async fn a_store_writer_whose_guard_is_false_writes_nothing() {
        with_temp_data_root(async {
            let mut run = writer_run(
                vec![
                    writer_step(
                        "lock",
                        "session_put",
                        Some("locked_intent"),
                        serde_json::json!({"value": "intent_out"}),
                        Some(("intent_out", "eq", Some("network"))),
                    ),
                    writer_step(
                        "open",
                        "case_open",
                        Some("nc"),
                        serde_json::json!({"entity": "intent_out", "label": "intent_out"}),
                        Some(("intent_out", "absent", None)),
                    ),
                    writer_step(
                        "hold",
                        "latch",
                        None,
                        serde_json::Value::Null,
                        Some(("never", "present", None)),
                    ),
                ],
                Some("chat-sg"),
                &[("intent_out", "billing")],
            );
            assert!(run.execute().await.unwrap().is_none());
            let mut conn = outputs(&run).await;
            for table in [
                "session_variables",
                "case_records",
                "case_entities",
                "case_strategies",
                "config_deltas",
            ] {
                let n: i64 = sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {table}"))
                    .fetch_one(&mut conn)
                    .await
                    .unwrap();
                assert_eq!(n, 0, "{table}");
            }
            assert_eq!(
                crate::persistence::session_store::write_mode(&mut conn, "chat-sg")
                    .await
                    .unwrap(),
                crate::persistence::session_store::WriteMode::Open
            );
            assert!(
                run.store_vars.is_empty(),
                "a skipped open published nothing"
            );
        })
        .await;
    }

    /// items.id=889: a row runs only when EVERY clause holds, the single guard
    /// and each one of guard_clauses, evaluated at run time over the live track.
    #[tokio::test]
    async fn an_and_guarded_row_runs_only_when_every_clause_holds() {
        with_temp_data_root(async {
            let mk = |id: &str,
                      op: &str,
                      target: Option<&str>,
                      args: serde_json::Value,
                      single: (&str, &str, Option<&str>),
                      clauses: &[(&str, &str, Option<&str>)]| {
                let mut s = writer_step(id, op, target, args, Some(single));
                s.and_guards = clauses
                    .iter()
                    .map(|(v, o, val)| {
                        guard::Guard::from_columns(Some(v), Some(o), *val)
                            .unwrap()
                            .unwrap()
                    })
                    .collect();
                s
            };
            let mut run = writer_run(
                vec![
                    // Both hold: writes.
                    mk(
                        "lock",
                        "session_put",
                        Some("locked_intent"),
                        serde_json::json!({"value": "intent_out"}),
                        ("intent_out", "eq", Some("billing")),
                        &[("stage", "eq", Some("go"))],
                    ),
                    // The single guard holds, a LIST clause does not.
                    mk(
                        "open",
                        "case_open",
                        Some("nc"),
                        serde_json::json!({"entity": "intent_out", "label": "intent_out"}),
                        ("intent_out", "eq", Some("billing")),
                        &[("stage", "eq", Some("stop"))],
                    ),
                    // The single guard does not hold, the list does.
                    mk(
                        "hold",
                        "latch",
                        None,
                        serde_json::Value::Null,
                        ("intent_out", "eq", Some("network")),
                        &[("stage", "eq", Some("go"))],
                    ),
                    // The second of three clauses does not hold.
                    mk(
                        "open2",
                        "case_open",
                        Some("nc2"),
                        serde_json::json!({"entity": "intent_out", "label": "intent_out"}),
                        ("intent_out", "present", None),
                        &[
                            ("stage", "in", Some(r#"["x","y"]"#)),
                            ("intent_out", "ne", Some("z")),
                        ],
                    ),
                ],
                Some("chat-and"),
                &[("intent_out", "billing"), ("stage", "go")],
            );
            assert!(run.execute().await.unwrap().is_none());
            let mut conn = outputs(&run).await;
            let rows = crate::persistence::session_store::list_session_variables(
                &mut conn, "chat-and", "f",
            )
            .await
            .unwrap();
            assert_eq!(
                rows.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
                ["locked_intent"],
                "only the row whose every clause held wrote"
            );
            let cases: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM case_records")
                .fetch_one(&mut conn)
                .await
                .unwrap();
            assert_eq!(cases, 0);
            assert!(
                run.store_vars.is_empty(),
                "a skipped open published nothing"
            );
            assert_eq!(
                crate::persistence::session_store::write_mode(&mut conn, "chat-and")
                    .await
                    .unwrap(),
                crate::persistence::session_store::WriteMode::Open,
                "the latch row's second clause was on a false single guard"
            );
        })
        .await;
    }

    /// A bound name used ONLY in a clause of the list is still seeded at pass
    /// start (seeding reads exactly the names the row set mentions).
    #[tokio::test]
    async fn a_bound_name_named_only_in_a_clause_list_is_seeded() {
        with_temp_data_root(async {
            let mut step = writer_step(
                "lock",
                "session_put",
                Some("locked_intent"),
                serde_json::json!({"value": "intent_out"}),
                None,
            );
            step.and_guards =
                vec![
                    guard::Guard::from_columns(Some("write_mode"), Some("eq"), Some("open"))
                        .unwrap()
                        .unwrap(),
                ];
            let mut run = writer_run(
                vec![step],
                Some("chat-seed-and"),
                &[("intent_out", "billing")],
            );
            assert!(run.execute().await.unwrap().is_none());
            let mut conn = outputs(&run).await;
            let row = crate::persistence::session_store::get_session_variable(
                &mut conn,
                "chat-seed-and",
                "f",
                "locked_intent",
            )
            .await
            .unwrap();
            assert!(
                row.is_some(),
                "write_mode was seeded for the clause, so it held"
            );
        })
        .await;
    }

    /// case_select through execute(): a handle a model produced resolves
    /// store-side, a made-up one fails the pass closed and writes nothing.
    #[tokio::test]
    async fn execute_selects_a_ticket_by_handle_and_a_wrong_handle_fails_the_pass() {
        with_temp_data_root(async {
            let select = || {
                writer_step(
                    "sel",
                    "case_select",
                    None,
                    serde_json::json!({"handle": "ticket_handle"}),
                    None,
                )
            };
            // A case this Focus opened, in the run's own persona database.
            let mut run = writer_run(
                vec![select()],
                Some("chat-hnd"),
                &[("ticket_handle", "T-00000000")],
            );
            let mut conn = outputs(&run).await;
            crate::persistence::case_store::open_case(
                &mut conn,
                &crate::persistence::case_store::NewCase {
                    case_id: "run-0:open",
                    focus_id: "f",
                    context_key: "chat-hnd",
                    primary_entity_id: "ent-router",
                    subject_label: "Router",
                    symptoms: "",
                    opened_by_run: "run-0",
                },
            )
            .await
            .unwrap();
            drop(conn);

            // A made-up handle: the pass fails closed, nothing is selected.
            let err = run.execute().await.unwrap_err();
            assert!(matches!(err, LifecycleError::StoreWrite(_)), "{err:?}");
            assert!(!err.to_string().contains("T-00000000"), "{err}");
            let mut conn = outputs(&run).await;
            let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM session_variables")
                .fetch_one(&mut conn)
                .await
                .unwrap();
            assert_eq!(n, 0);
            drop(conn);

            // The real handle selects the case.
            let handle = crate::persistence::handles::case_handle("f", "run-0:open");
            let mut run2 = writer_run(
                vec![select()],
                Some("chat-hnd"),
                &[("ticket_handle", handle.as_str())],
            );
            run2.user_id = run.user_id.clone();
            run2.persona_id = run.persona_id.clone();
            assert!(run2.execute().await.unwrap().is_none());
            let mut conn = outputs(&run2).await;
            let row = crate::persistence::session_store::get_session_variable(
                &mut conn,
                "chat-hnd",
                "f",
                "active_case_id",
            )
            .await
            .unwrap()
            .expect("selected");
            assert_eq!(row.value, "run-0:open");
        })
        .await;
    }

    #[tokio::test]
    async fn a_latched_chat_pass_goes_on_and_a_writer_writes_nothing() {
        with_temp_data_root(async {
            let mut run = writer_run(
                vec![
                    writer_step(
                        "lock",
                        "session_put",
                        Some("locked_intent"),
                        serde_json::json!({"value": "intent_out"}),
                        None,
                    ),
                    hand_built_cb06_step_after_draft(),
                ],
                Some("chat-sl"),
                &[("intent_out", "billing"), ("draft_output", "a draft")],
            );
            let mut conn = outputs(&run).await;
            crate::persistence::session_store::latch_write_suppressed(
                &mut conn, "chat-sl", "f", "run-x",
            )
            .await
            .unwrap();
            drop(conn);

            assert!(
                run.execute().await.unwrap().is_none(),
                "the pass is not failed"
            );
            assert!(
                ran_ids(&run).contains(&"quality_check".to_owned()),
                "the row after the refused writer still ran: {:?}",
                ran_ids(&run)
            );
            let mut conn = outputs(&run).await;
            assert!(crate::persistence::session_store::get_session_variable(
                &mut conn,
                "chat-sl",
                "f",
                "locked_intent",
            )
            .await
            .unwrap()
            .is_none());
        })
        .await;
    }

    fn hand_built_cb06_step_after_draft() -> ActionDefinition {
        hand_built_cb06_step()
    }

    /// The latch row in a pass closes every later writer of the SAME pass, even
    /// though write_mode was seeded open at pass start.
    #[tokio::test]
    async fn a_latch_row_closes_the_writers_after_it_in_the_same_pass() {
        with_temp_data_root(async {
            let mut run = writer_run(
                vec![
                    writer_step("hold", "latch", None, serde_json::Value::Null, None),
                    writer_step(
                        "lock",
                        "session_put",
                        Some("locked_intent"),
                        serde_json::json!({"value": "intent_out"}),
                        None,
                    ),
                ],
                Some("chat-slr"),
                &[("intent_out", "billing")],
            );
            assert!(run.execute().await.unwrap().is_none());
            let mut conn = outputs(&run).await;
            assert_eq!(
                crate::persistence::session_store::write_mode(&mut conn, "chat-slr")
                    .await
                    .unwrap(),
                crate::persistence::session_store::WriteMode::Suppressed
            );
            assert!(crate::persistence::session_store::get_session_variable(
                &mut conn,
                "chat-slr",
                "f",
                "locked_intent",
            )
            .await
            .unwrap()
            .is_none());
        })
        .await;
    }

    /// A failing writer ends the pass: the error comes back, and no later row of
    /// the pass runs or writes.
    #[tokio::test]
    async fn a_failing_writer_fails_the_pass_closed_and_later_rows_do_not_run() {
        with_temp_data_root(async {
            let mut run = writer_run(
                vec![
                    // "Not Valid!" is not an intent identifier.
                    writer_step(
                        "bad",
                        "session_put",
                        Some("locked_intent"),
                        serde_json::json!({"value": "intent_out"}),
                        None,
                    ),
                    writer_step(
                        "after",
                        "session_put",
                        Some("active_case_id"),
                        serde_json::json!({"value": "case_out"}),
                        None,
                    ),
                ],
                Some("chat-sf"),
                &[("intent_out", "Not Valid!"), ("case_out", "case-1")],
            );
            let err = run.execute().await.unwrap_err();
            assert!(matches!(err, LifecycleError::StoreWrite(_)), "{err:?}");
            assert!(
                !err.to_string().contains("Not Valid"),
                "no value in the error: {err}"
            );
            let mut conn = outputs(&run).await;
            let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM session_variables")
                .fetch_one(&mut conn)
                .await
                .unwrap();
            assert_eq!(n, 0, "the row after the failure never ran");
        })
        .await;
    }

    /// A run that belongs to no chat cannot write: it fails closed.
    #[tokio::test]
    async fn a_run_with_no_chat_fails_closed_at_a_store_writer() {
        with_temp_data_root(async {
            let mut run = writer_run(
                vec![writer_step(
                    "lock",
                    "session_put",
                    Some("locked_intent"),
                    serde_json::json!({"value": "intent_out"}),
                    None,
                )],
                None,
                &[("intent_out", "billing")],
            );
            let err = run.execute().await.unwrap_err();
            assert!(
                matches!(
                    err,
                    LifecycleError::StoreWrite(
                        crate::conductor::store_writer::StoreWriteError::NoContextKey
                    )
                ),
                "{err:?}"
            );
        })
        .await;
    }

    /// An id one writer publishes is an argument and a guard variable for a later
    /// writer in the same pass, and the case carries the running Focus.
    #[tokio::test]
    async fn a_published_id_reaches_a_later_writer_and_guard_in_the_same_pass() {
        with_temp_data_root(async {
            let mut run = writer_run(
                vec![
                    writer_step(
                        "open",
                        "case_open",
                        Some("new_case"),
                        serde_json::json!({"entity": "ent_out", "label": "label_out"}),
                        None,
                    ),
                    writer_step(
                        "log",
                        "strategy_append",
                        None,
                        serde_json::json!({"case": "new_case", "strategy": "s_out", "outcome": "o_out"}),
                        Some(("new_case", "present", None)),
                    ),
                ],
                Some("chat-sp"),
                &[
                    ("ent_out", "ent-router"),
                    ("label_out", "Router"),
                    ("s_out", "restart it"),
                    ("o_out", "no change"),
                ],
            );
            assert!(run.execute().await.unwrap().is_none());
            let run_id = run.focus_run_id.clone().unwrap();
            let case_id = format!("{run_id}:open");
            assert_eq!(run.store_vars.get("new_case"), Some(&case_id));
            let mut conn = outputs(&run).await;
            let case = crate::persistence::case_store::get_case(&mut conn, &case_id)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(case.focus_id, "f");
            let strategies =
                crate::persistence::case_store::list_case_strategies(&mut conn, &case_id)
                    .await
                    .unwrap();
            assert_eq!(strategies.len(), 1);
            assert_eq!(strategies[0].rung_at, Some(1));
            // The published id is not on the track, so it cannot reach {previous_output}.
            assert!(!ran_ids(&run).iter().any(|a| a == "open" || a == "log"));
        })
        .await;
    }

    /// A session-bound name is seeded because a writer names it as an argument
    /// (seeding.required_names), and reaches the writer.
    #[tokio::test]
    async fn a_seeded_session_name_reaches_a_writer_as_an_argument() {
        with_temp_data_root(async {
            let mut run = writer_run(
                vec![writer_step(
                    "advance",
                    "case_update",
                    None,
                    serde_json::json!({"case": "active_case_id", "outcome": "o_out"}),
                    Some(("active_case_id", "present", None)),
                )],
                Some("chat-ss"),
                &[("o_out", "restarted the router")],
            );
            // Plant the case and the active_case_id through the typed store.
            let run_id = run.focus_run_id.clone().unwrap();
            let mut conn = outputs(&run).await;
            crate::persistence::case_store::open_case(
                &mut conn,
                &crate::persistence::case_store::NewCase {
                    case_id: "case-seeded",
                    focus_id: "f",
                    context_key: "chat-ss",
                    primary_entity_id: "ent-router",
                    subject_label: "Router",
                    symptoms: "",
                    opened_by_run: &run_id,
                },
            )
            .await
            .unwrap();
            crate::persistence::session_store::put_session_variable(
                &mut conn,
                &crate::persistence::session_store::SessionWrite {
                    context_key: "chat-ss",
                    focus_id: "f",
                    name: "active_case_id",
                    value: "case-seeded",
                    written_by_run: &run_id,
                    expected_version: None,
                },
            )
            .await
            .unwrap();
            drop(conn);

            assert!(run.execute().await.unwrap().is_none());
            let mut conn = outputs(&run).await;
            let case = crate::persistence::case_store::get_case(&mut conn, "case-seeded")
                .await
                .unwrap()
                .unwrap();
            assert_eq!(case.outcome.as_deref(), Some("restarted the router"));
        })
        .await;
    }
}

// ---------------------------------------------------------------------------
// items.id=716: the shipped api-chat Focus
// ---------------------------------------------------------------------------

#[cfg(test)]
mod api_chat_focus_tests {
    use super::*;

    const API_CHAT_FOCUS: &str = include_str!("../../../app/core_artifacts/focuses/api-chat.focus");

    fn parsed() -> FocusDefinition {
        parse_focus_text(API_CHAT_FOCUS).expect("api-chat.focus must parse")
    }

    #[test]
    fn it_is_one_external_step_with_no_handoff_and_no_override() {
        let def = parsed();
        assert_eq!(def.focus_id, crate::api_chat::API_CHAT_FOCUS_ID);
        assert_eq!(def.max_routing_tier, ExternalAccess::Unrestricted);
        assert_eq!(def.steps.len(), 1);
        let step = &def.steps[0];
        assert_eq!(step.routing_tier, ExternalAccess::Unrestricted);
        assert!(
            !step.requires_user_handoff,
            "routing_tier 3 alone would imply a handoff"
        );
        assert_eq!(
            step.external_access_override, None,
            "an unrestricted step carries no override"
        );
        assert!(
            step.field_requirements.is_empty(),
            "no personal field reaches the provider"
        );
        assert_eq!(def.output_type, "api_chat");
    }

    #[test]
    fn the_prompt_carries_only_the_conversation() {
        let step = &parsed().steps[0];
        assert!(step.prompt_template.contains("{user_input}"));
        for token in ["{persona_context}", "{voice_profile}", "{previous_output}"] {
            assert!(
                !step.prompt_template.contains(token),
                "{token} must not ride on an API Chat prompt"
            );
        }
        // No output limit here: the model row's limit governs the hosted call
        // (the spend gate's worst case depends on it).
        assert!(!step.options_override.contains_key("num_predict"));
    }

    #[test]
    fn it_declares_its_own_guide_and_never_uses_the_retired_default() {
        let def = parsed();
        assert!(def.steps.iter().all(|s| s.guide_id == "api-chat-guide"));
        assert!(
            !API_CHAT_FOCUS.to_lowercase().contains("quick"),
            "Quick Ask is a retired concept (decisions.id=804)"
        );
    }
}

// ---------------------------------------------------------------------------
// items.id=617: Focus file search roots
// ---------------------------------------------------------------------------

#[cfg(test)]
mod focus_search_tests {
    use super::*;

    fn quick_ask_text() -> String {
        std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../app/core_artifacts/focuses/quick-ask.focus"),
        )
        .expect("quick-ask.focus is tracked in the repo")
    }

    #[test]
    fn bundled_dir_resolves_quick_ask() {
        let bundled = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        std::fs::write(bundled.path().join("quick-ask.focus"), quick_ask_text()).unwrap();

        let found =
            find_focus_file_in("quick-ask", Some(bundled.path()), data.path(), None).unwrap();
        assert_eq!(found, bundled.path().join("quick-ask.focus"));
        let def = parse_focus_text(&std::fs::read_to_string(found).unwrap()).unwrap();
        assert_eq!(def.focus_id, "quick-ask");
    }

    #[test]
    fn bundled_dir_wins_over_community_and_dev() {
        let bundled = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let dev = tempfile::tempdir().unwrap();
        let community = data.path().join("community_artifacts/focuses");
        std::fs::create_dir_all(&community).unwrap();
        for dir in [bundled.path(), community.as_path(), dev.path()] {
            std::fs::write(dir.join("quick-ask.focus"), "x").unwrap();
        }

        let found = find_focus_file_in(
            "quick-ask",
            Some(bundled.path()),
            data.path(),
            Some(dev.path()),
        )
        .unwrap();
        assert_eq!(found, bundled.path().join("quick-ask.focus"));

        std::fs::remove_file(bundled.path().join("quick-ask.focus")).unwrap();
        let found = find_focus_file_in(
            "quick-ask",
            Some(bundled.path()),
            data.path(),
            Some(dev.path()),
        )
        .unwrap();
        assert_eq!(found, community.join("quick-ask.focus"));

        std::fs::remove_file(community.join("quick-ask.focus")).unwrap();
        let found = find_focus_file_in(
            "quick-ask",
            Some(bundled.path()),
            data.path(),
            Some(dev.path()),
        )
        .unwrap();
        assert_eq!(found, dev.path().join("quick-ask.focus"));
    }

    #[test]
    fn missing_everywhere_is_focus_not_found() {
        let data = tempfile::tempdir().unwrap();
        let err = find_focus_file_in("quick-ask", None, data.path(), None).unwrap_err();
        assert!(matches!(err, LifecycleError::FocusNotFound(id) if id == "quick-ask"));
    }

    /// Debug builds keep the source-tree fallback that `cargo tauri dev` needs.
    #[cfg(debug_assertions)]
    #[test]
    fn debug_build_consults_the_source_tree() {
        let dev = dev_focus_dir().expect("debug builds have a dev dir");
        assert!(dev.join("quick-ask.focus").exists());
    }

    /// A release build must never consult the build machine's path: with no
    /// bundled dir and an empty data root, quick-ask is not found even though
    /// the file exists in this checkout.
    #[cfg(not(debug_assertions))]
    #[tokio::test]
    async fn release_build_never_consults_the_build_machine_path() {
        let _lock = crate::test_support::ENV_MUTEX.lock().await;
        let data = tempfile::tempdir().unwrap();
        std::env::set_var("QR_DATA_ROOT", data.path());

        assert!(dev_focus_dir().is_none());
        assert!(FOCUS_DIR.get().is_none(), "no test sets the global");
        assert!(std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../app/core_artifacts/focuses/quick-ask.focus")
            .exists());
        let err = find_focus_file("quick-ask").unwrap_err();
        assert!(matches!(err, LifecycleError::FocusNotFound(_)));

        std::env::remove_var("QR_DATA_ROOT");
    }
}
