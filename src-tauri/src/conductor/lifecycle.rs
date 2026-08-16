// src-tauri/src/conductor/lifecycle.rs
//
// FocusRun — Conductor execution engine's seven-phase lifecycle.
// FocusDefinition — parsed .focus YAML file representation.
// RunResult — structured outcome from a completed or interrupted run.
// demote_interrupted_runs() — standalone async fn for startup recovery.
//
// Phase 1 LOAD:       parse .focus file via serde_yaml; validate steps
// Phase 2 AUTHORIZE:  focus_settings tier check; create focus_run record (initializing)
// Phase 3 INITIALIZE: build + seal PersonalTrack; assemble TaskTrack + SharedStateTrack;
//                     construct PrivacyGateway; assemble persona context (async);
//                     promote focus_run to running
// Phase 4 EXECUTE:    step loop — Tier 3 steps are terminal boundaries;
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
// serde_yaml replaces Python hand-parser:
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
//   life_context -> persona_context (D6-323, token name; DB field name retained)
//   focus_runs, focus_run_snapshots (SQL table identifiers)

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::ConnectOptions;
use sqlx::Row;
use sqlx::SqliteConnection;
use thiserror::Error;
use uuid::Uuid;

use crate::conductor::concurrency::ConductorScheduler;
use crate::conductor::executor::{StepContext, StepExecutor};
use crate::conductor::failure::{
    ConductorError, FailureAction, FailureHandler, FailureResult, FailureSeverity,
};
use crate::conductor::memory_broker::MemoryBroker;
use crate::conductor::privacy::{logger::DisclosureLoggerForRun, PrivacyGateway};
use crate::conductor::tokens::{validate_step, FieldRequirement, StepDefinition, StepType};
use crate::conductor::types::{
    PersonalContextManifest, PersonalTrack, SharedStateTrack, TaskTrack,
};
use crate::persistence::disclosure_log_store::SqliteDisclosureLogger;
use crate::providers::utils::{
    connect_options_encrypted, connect_options_unencrypted, db_path_outputs, db_path_shared, now,
};

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
    // Phase 1 — LOAD
    #[error("Focus file not found: {0}.focus")]
    FocusNotFound(String),
    #[error("YAML parse error: {0}")]
    YamlParse(#[from] serde_yaml::Error),
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
    #[error("Persona store: {0}")]
    PersonaStore(String),
    #[error("Focus settings store: {0}")]
    FocusSettingsStore(String),
    #[error("Topic store: {0}")]
    TopicStore(String),
    #[error("JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("IO: {0}")]
    Io(#[from] std::io::Error),
}

// ---------------------------------------------------------------------------
// Internal serde_yaml deserialization structs (private)
// ---------------------------------------------------------------------------
// These map 1:1 to the .focus YAML file structure. parse_focus_definition()
// converts raw -> domain types. brief: block is parsed but discarded (D6-343).

#[derive(Deserialize)]
struct RawFocusFile {
    id: Option<String>,
    display_name: Option<String>,
    description: Option<String>,
    version: Option<serde_yaml::Value>, // YAML may parse "1.0" as Number; stringify
    max_routing_tier: Option<u8>,
    output_types: Option<Vec<String>>, // conductor-brief (preferred)
    output_type: Option<String>,       // legacy single value — fallback only
    guides: Option<Vec<String>>,       // focus-level; first entry = step default
    suggest_in_focuses: Option<Vec<String>>,
    multi_source_validation: Option<bool>,
    steps: Option<IndexMap<String, RawStep>>, // IndexMap preserves YAML dict order
    #[allow(dead_code)] // D6-343: parsed for forward compat, not wired in Release 1.
    brief: Option<serde_yaml::Value>,
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
    step_type: Option<String>,
    output_var: Option<String>,
    prompt_template: Option<String>,
    field_requirements: Option<Vec<RawFieldRequirement>>,
    options_override: Option<serde_yaml::Value>, // YAML map -> HashMap<String, json::Value>
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
        _ => Err(format!(
            "invalid offset unit in {raw:?} (expected h or d)"
        )),
    }
}

/// Convert RawFocusFile -> FocusDefinition.
/// Replaces Python's hand-written _parse_focus_definition() with serde_yaml.
/// Applies the same shims: guide_id inheritance, output_types->output_type,
/// field_requirements list -> HashMap.
fn parse_focus_definition(raw: RawFocusFile) -> Result<FocusDefinition, LifecycleError> {
    let focus_id = raw.id.unwrap_or_default();

    // Focus-level guides list: first entry is the step default (COMPATIBILITY shim).
    // conductor-brief declares guides at focus level; StepDefinition still requires
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
        Some(serde_yaml::Value::String(s)) => s,
        Some(serde_yaml::Value::Number(n)) => n.to_string(),
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

    let mut steps: Vec<StepDefinition> = Vec::new();
    if let Some(raw_steps) = raw.steps {
        for (step_id_key, raw_step) in raw_steps {
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

            let step_type = raw_step
                .step_type
                .as_deref()
                .and_then(|s| s.parse::<StepType>().ok())
                .unwrap_or_default();

            // options_override: YAML map -> HashMap<String, serde_json::Value>.
            // serde_yaml::Value implements Serialize; round-trip via serde_json is safe.
            let options_override = raw_step
                .options_override
                .and_then(|v| serde_json::to_value(v).ok())
                .and_then(|jv| {
                    if let serde_json::Value::Object(map) = jv {
                        Some(map.into_iter().collect::<HashMap<_, _>>())
                    } else {
                        None
                    }
                })
                .unwrap_or_default();

            steps.push(StepDefinition {
                step_id: step_id_key.clone(),
                display_name: raw_step.display_name.unwrap_or_else(|| step_id_key.clone()),
                guide_id: raw_step
                    .guide_id
                    .unwrap_or_else(|| default_guide_id.clone()),
                task_type: raw_step.task_type.unwrap_or_else(|| "general".to_owned()),
                routing_tier: raw_step.routing_tier.unwrap_or(1),
                step_type,
                output_var: raw_step.output_var,
                prompt_template: raw_step.prompt_template.unwrap_or_default(),
                field_requirements,
                options_override,
            });
        }
    }

    Ok(FocusDefinition {
        display_name: raw.display_name.unwrap_or_else(|| focus_id.clone()),
        description: raw.description.unwrap_or_default(),
        max_routing_tier: raw.max_routing_tier.unwrap_or(1),
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

/// Parse and validate a .focus YAML file by focus_id — no DB access, no
/// FocusRun construction required. Extracted from FocusRun::load() (items.
/// id=236) so callers that only need a FocusDefinition (e.g.
/// commands::active_board::get_active_board) aren't forced to construct a
/// full FocusRun (11 constructor args) just to read display_config.
pub async fn load_focus_definition(focus_id: &str) -> Result<FocusDefinition, LifecycleError> {
    let focus_file = find_focus_file(focus_id)?;
    let text = tokio::fs::read_to_string(&focus_file).await?;
    let raw: RawFocusFile = serde_yaml::from_str(&text)?;
    let focus_def = parse_focus_definition(raw)?;

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

fn find_focus_file(focus_id: &str) -> Result<PathBuf, LifecycleError> {
    // Two search locations. In Tauri production, focuses are embedded via
    // Tauri resources config (bundling TBD). Until bundled, resolve relative
    // to CWD (dev workflow — matches Python oracle's repo-relative path).
    let data_root = crate::providers::utils::get_data_root();
    let filename = format!("{focus_id}.focus");

    let candidates = [
        PathBuf::from("app")
            .join("core_artifacts")
            .join("focuses")
            .join(&filename),
        data_root
            .join("community_artifacts")
            .join("focuses")
            .join(&filename),
    ];

    for candidate in &candidates {
        if candidate.exists() {
            return Ok(candidate.clone());
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
    pub max_routing_tier: u8,
    pub steps: Vec<StepDefinition>,
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
    let path = db_path_outputs(user_id, persona_id);

    if !path.exists() {
        crate::persistence::migrations::migrate_outputs_db(user_id, persona_id, key_hex)
            .await
            .map_err(|e| LifecycleError::DatabaseMigration(e.to_string()))?;
    }

    let conn = connect_options_encrypted(&path, key_hex)
        .create_if_missing(false)
        .connect()
        .await?;
    Ok(conn)
}

/// Open shared.db (unencrypted) for artifact version queries and floor consent reads.
/// Python oracle: open_instance_db() context manager.
async fn open_instance_db() -> Result<SqliteConnection, LifecycleError> {
    let path = db_path_shared();
    let conn = connect_options_unencrypted(&path).connect().await?;
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

    // State populated during lifecycle phases (all Option — None until populated)
    pub focus_run_id: Option<String>,
    pub focus_def: Option<FocusDefinition>,
    pub personal_track: Option<PersonalTrack>,
    pub task_track: Option<TaskTrack>,
    pub shared_state: Option<SharedStateTrack>,
    pub failure_handler: Option<FailureHandler>,
    pub privacy_gateway: Option<PrivacyGateway<L>>,

    // Tier configuration (set at AUTHORIZE, used throughout EXECUTE)
    _focus_max_permitted_tier: u8,
    _focus_privacy_tier: u8,

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
}

impl<L: DisclosureLoggerForRun> FocusRun<L> {
    #[allow(clippy::too_many_arguments)] // Explicit architecture boundary; see D6-342/D6-346.
    pub fn new(
        user_id: String,
        persona_id: String,
        focus_id: String,
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
            scheduler,
            user_input,
            is_fast_lane,
            key_hex,
            topic_id,
            is_quick_ask,
            confirmed_cross_persona_fact_ids,
            app_handle,
            focus_run_id: None,
            focus_def: None,
            personal_track: None,
            task_track: None,
            shared_state: None,
            failure_handler: None,
            privacy_gateway: None,
            _focus_max_permitted_tier: 1,
            _focus_privacy_tier: 1,
            _output_id: None,
            current_step: 0,
            _checkpointing_suspended: false,
            _persona_context_rendered: String::new(),
            crisis_floor_triggered: false,
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
        self.emit_status_with_content(status, step_display_name, None);
    }

    /// step_content: the just-completed step's real generated content
    /// (TaskStep.content, via task_track.last_output()). Only the
    /// step-completion call site in execute() passes Some(...); every other
    /// caller (including plain emit_status above) passes None.
    fn emit_status_with_content(
        &self,
        status: &str,
        step_display_name: Option<&str>,
        step_content: Option<&str>,
    ) {
        let Some(handle) = &self.app_handle else {
            return; // no handle in tests — silent no-op
        };
        let total = self.focus_def.as_ref().map(|d| d.steps.len()).unwrap_or(0);
        let payload = RunStatusPayload {
            focus_run_id: self.focus_run_id.clone().unwrap_or_default(),
            status: status.to_owned(),
            current_step: self.current_step,
            total_steps: total,
            step_display_name: step_display_name.map(|s| s.to_owned()),
            step_content: step_content.map(|s| s.to_owned()),
        };
        use tauri::Emitter;
        if let Err(e) = handle.emit("run-status-update", &payload) {
            log::warn!("lifecycle: emit run-status-update failed: {e}");
        }
    }

    // =========================================================================
    // Phase 1 — LOAD
    // =========================================================================

    /// Parse the .focus YAML file and validate all steps.
    /// Populates self.focus_def. No DB access.
    /// Python oracle: FocusRun.load()
    pub async fn load(&mut self) -> Result<(), LifecycleError> {
        self.focus_def = Some(load_focus_definition(&self.focus_id).await?);
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

        let (max_permitted, privacy_tier) = self.get_focus_tier_ceiling().await?;
        self._focus_max_permitted_tier = max_permitted;
        self._focus_privacy_tier = privacy_tier;

        let focus_def = self.focus_def.as_ref().unwrap();
        for step in &focus_def.steps {
            if step.routing_tier > self._focus_max_permitted_tier {
                return Err(LifecycleError::TierViolation(format!(
                    "Step '{}' requires tier {} but focus ceiling is {}.",
                    step.step_id, step.routing_tier, self._focus_max_permitted_tier
                )));
            }
        }

        self.failure_handler = Some(FailureHandler::new(self._focus_max_permitted_tier));
        self.focus_run_id = Some(Uuid::new_v4().to_string());
        self.write_focus_run_record("initializing").await?;
        Ok(())
    }

    /// Read (max_permitted_tier, privacy_tier) from focus_settings.
    /// Asserts focus_settings row exists — missing row is a hard error (D6-303).
    /// Python oracle: FocusRun._get_focus_tier_ceiling()
    async fn get_focus_tier_ceiling(&self) -> Result<(u8, u8), LifecycleError> {
        use crate::persistence::focus_settings_store::get_focus_settings;
        use crate::persistence::persona_store::get_persona_for_user;

        let _persona = get_persona_for_user(&self.user_id, &self.persona_id)
            .await
            .map_err(|e| LifecycleError::PersonaStore(e.to_string()))?
            .ok_or_else(|| {
                LifecycleError::PersonaNotFound(format!(
                    "Persona '{}' not found for user '{}'.",
                    self.persona_id, self.user_id
                ))
            })?;

        let settings = get_focus_settings(&self.persona_id, &self.focus_id)
            .await
            .map_err(|e| LifecycleError::FocusSettingsStore(e.to_string()))?
            .ok_or_else(|| {
                LifecycleError::FocusSettingsNotFound(format!(
                    "Focus settings not found for persona='{}' focus='{}'. \
                     Configure this Focus before running.",
                    self.persona_id, self.focus_id
                ))
            })?;

        Ok((
            settings.max_permitted_tier as u8,
            settings.privacy_tier as u8,
        ))
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
                self._focus_max_permitted_tier as i32,
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
            // No emit() here on purpose: nothing listens yet, and naming an
            // event now would prejudge decisions.id=639's still-unbuilt
            // frontend confirmation flow.
            if let Some(gateway) = self.privacy_gateway.as_ref() {
                use crate::conductor::privacy::logger::DisclosureLogEntry;
                let entry = DisclosureLogEntry {
                    step_id: "initialize".to_string(),
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
        let Ok(mut conn) = open_instance_db().await else {
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
        match q.fetch_all(&mut conn).await {
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
        let Ok(mut conn) = open_instance_db().await else {
            return IndexMap::new();
        };
        match sqlx::query(
            "SELECT artifact_id, version FROM artifact_versions \
             WHERE artifact_type = 'operator' AND revoked = 0",
        )
        .fetch_all(&mut conn)
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

    /// Run the step loop. Returns Some(RunResult) on early exit (Tier 3 boundary
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

            // Clone step to release focus_def borrow before &mut self calls below.
            let step = self.focus_def.as_ref().unwrap().steps[i].clone();

            self.emit_status("running", Some(&step.display_name));

            // Tier 3 boundary: checkpoint (if not suspended), set status, return early.
            if step.routing_tier == 3 {
                if !self._checkpointing_suspended {
                    if let Err(e) = self.write_checkpoint(&step.step_id).await {
                        log::warn!("lifecycle: Tier 3 checkpoint write failed: {e}");
                    }
                }
                let _ = self.write_focus_run_record("awaiting_user").await;
                self.emit_status("awaiting_user", Some(&step.display_name));
                return Ok(Some(RunResult {
                    focus_run_id: self.focus_run_id.clone().unwrap_or_default(),
                    status: "awaiting_user".to_owned(),
                    output_id: None,
                    output_content: None,
                    failure: None,
                }));
            }

            let step_failure = self.execute_step(&step, i).await?;

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
            let step_content = self
                .task_track
                .as_ref()
                .and_then(|tt| tt.last_output())
                .map(|s| s.to_owned());
            self.emit_status_with_content(
                "running",
                Some(&step.display_name),
                step_content.as_deref(),
            );

            checkpoint_counter += 1;
            if checkpoint_counter >= checkpoint_every && !self._checkpointing_suspended {
                if let Err(e) = self.write_checkpoint(&step.step_id).await {
                    log::warn!("lifecycle: periodic checkpoint write failed: {e}");
                }
                checkpoint_counter = 0;
            }
        }

        Ok(None) // All steps completed — proceed to Phase 5 OUTPUT
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
        step: &StepDefinition,
        step_index: usize,
    ) -> Result<Option<FailureResult>, LifecycleError> {
        // Axis 1: execution_tier — min(focus_max_permitted, focus_max_routing, step.routing_tier)
        let execution_tier = {
            let fd = self.focus_def.as_ref().unwrap();
            u8::min(
                u8::min(self._focus_max_permitted_tier, fd.max_routing_tier),
                step.routing_tier,
            )
        };

        // Axis 2: abstraction_tier with floor clamping (ADR-012 Amendment 3)
        let raw_abstraction = self._focus_privacy_tier.min(execution_tier);
        let abstraction_tier = if execution_tier > 1 {
            raw_abstraction.max(2) // floor clamp: abstraction_tier >= 2 when Tier 2+
        } else {
            raw_abstraction
        };

        log::debug!(
            "lifecycle: step={} execution_tier={} abstraction_tier={} \
             raw_abstraction={} focus_privacy_tier={}",
            step.step_id,
            execution_tier,
            abstraction_tier,
            raw_abstraction,
            self._focus_privacy_tier,
        );

        // Gate3 look-ahead: next step's execution_tier
        let next_execution_tier: Option<u8> = {
            let fd = self.focus_def.as_ref().unwrap();
            if step_index + 1 < fd.steps.len() {
                let next = &fd.steps[step_index + 1];
                Some(u8::min(
                    u8::min(self._focus_max_permitted_tier, fd.max_routing_tier),
                    next.routing_tier,
                ))
            } else {
                None
            }
        };

        // Clone values needed in the async block / StepContext before any &mut borrows.
        // Avoids holding references to self fields across the async block boundary.
        let persona_id = self.persona_id.clone();
        let focus_id = self.focus_id.clone();
        let focus_run_id = self.focus_run_id.clone().unwrap_or_default();
        let user_input = self.user_input.clone();
        let persona_context = self._persona_context_rendered.clone();
        let space_max_permitted_tier = self._focus_max_permitted_tier;
        let scheduler = Arc::clone(&self.scheduler);
        let user_id = self.user_id.clone();

        // Floor consent preference (D5-152). Read from personas.extra_metadata in shared.db.
        // Non-fatal — consent gate fires normally if read fails.
        let floor_consent_preference: Option<String> = async {
            let mut conn = open_instance_db().await.ok()?;
            let row = sqlx::query("SELECT extra_metadata FROM personas WHERE id = ?")
                .bind(&persona_id)
                .fetch_optional(&mut conn)
                .await
                .ok()??;
            let extra_str: String = row.try_get("extra_metadata").ok()?;
            let meta: serde_json::Value = serde_json::from_str(&extra_str).ok()?;
            let consent = meta.get("floor_consent_preference")?.as_object()?;
            let stored_mode = consent.get("mode")?.as_str()?;
            if stored_mode == "modified" {
                let stored_tier = consent.get("abstraction_tier")?.as_i64()? as u8;
                if stored_tier <= abstraction_tier {
                    Some("modified".to_owned())
                } else {
                    None
                }
            } else if stored_mode == "local" {
                Some("local".to_owned())
            } else {
                None
            }
        }
        .await;

        // Tier 2 provider preference (items.id=251). Only relevant at
        // tier>=2 -- Tier 1 never dispatches to an external provider. DB
        // read failure collapses to None, same as "no preference set" --
        // StepExecutor turns None into the F10 MissingTier2Config failure
        // rather than guessing a provider.
        let tier2_provider_preference: Option<String> = if execution_tier >= 2 {
            crate::auth::user_store::get_tier2_provider_preference(&user_id)
                .await
                .ok()
                .flatten()
        } else {
            None
        };

        let ctx = StepContext {
            step: step.clone(),
            focus_id,
            focus_run_id,
            user_input,
            persona_context,
            space_max_permitted_tier,
            execution_tier,
            abstraction_tier,
            raw_abstraction,
            floor_consent_preference,
            tier2_provider_preference,
            next_execution_tier,
            retry_count: 0,
            focus_name: self
                .focus_def
                .as_ref()
                .map(|d| d.display_name.clone())
                .unwrap_or_default(),
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
            )
            .await)
    }

    /// Update run status based on failure action; build RunResult.
    /// Python oracle: FocusRun._handle_step_failure()
    async fn handle_step_failure(
        &mut self,
        failure: FailureResult,
    ) -> Result<RunResult, LifecycleError> {
        if failure.action == FailureAction::Stop && !failure.is_recoverable {
            let _ = self.write_focus_run_record("failed").await;
            self.emit_status("failed", None);
        } else if matches!(
            failure.action,
            FailureAction::AwaitUser
                | FailureAction::HoldForGate
                | FailureAction::OfferTier2
                | FailureAction::OfferCompact
                | FailureAction::AwaitFloorConsent
                | FailureAction::AwaitConsent
        ) {
            let _ = self.write_focus_run_record("awaiting_user").await;
            self.emit_status("awaiting_user", None);
        }

        let status = self.get_current_status().await;
        Ok(RunResult {
            focus_run_id: self.focus_run_id.clone().unwrap_or_default(),
            status,
            output_id: None,
            output_content: None,
            failure: Some(failure),
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
    async fn write_checkpoint(&self, step_id: &str) -> Result<(), LifecycleError> {
        let personal_track = self.personal_track.as_ref().unwrap();
        let task_track = self.task_track.as_ref().unwrap();
        let shared_state = self.shared_state.as_ref().unwrap();
        let focus_run_id = self.focus_run_id.as_deref().unwrap();
        let key_hex = self.key_hex.as_deref().unwrap();

        // Task track JSON — matches Python oracle dict structure
        let task_data = serde_json::json!({
            "steps": task_track.steps().iter().map(|s| serde_json::json!({
                "step_id": s.step_id,
                "output_var": s.output_var,
                "content": s.content,
                "sensitivity_severity": s.sensitivity_severity,
                "routing_tier_used": s.routing_tier_used,
            })).collect::<Vec<_>>(),
            "sensitivity_ceiling": task_track.sensitivity_ceiling(),
        });

        // Shared state JSON — matches Python oracle dict structure
        let shared_data = serde_json::json!({
            "step_disclosure_buffers": shared_state.buffers(),
            "promotions": shared_state.promotions().iter().map(|p| serde_json::json!({
                "step_id": p.step_id,
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
             (id, focus_run_id, step_id, phase, task_track_json,
              shared_state_json, personal_context_manifest,
              checkpoint_hash, created_at)
             VALUES (?, ?, ?, 4, ?, ?, ?, ?, ?)",
        )
        .bind(Uuid::new_v4().to_string())
        .bind(focus_run_id)
        .bind(step_id)
        .bind(&task_json)
        .bind(&shared_json)
        .bind(&manifest_json)
        .bind(&checkpoint_hash)
        .bind(now())
        .execute(&mut conn)
        .await?;

        Ok(())
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
        // resource block. This is the run's single content-finalization
        // point, reached identically regardless of Persona/Focus/tier --
        // see crisis.rs module doc for the full reasoning, including the
        // known scope boundary (runs that pause or fail before reaching
        // OUTPUT do not currently surface this block).
        if self.crisis_floor_triggered {
            final_content.push_str("\n\n");
            final_content.push_str(&crate::conductor::crisis::resource_block());
        }

        let output_type = self.focus_def.as_ref().unwrap().output_type.clone();
        let focus_run_id = self.focus_run_id.clone().unwrap_or_default();
        let key_hex = self.key_hex.clone().unwrap_or_default();
        let sensitivity = self.output_sensitivity().to_owned();
        let output_id = Uuid::new_v4().to_string();

        save_output(
            &self.user_id,
            &self.persona_id,
            &key_hex,
            &focus_run_id,
            &output_type,
            &final_content,
            &sensitivity,
            Some(&output_id),
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
        let _ = self.write_focus_run_record(final_status).await;
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
                            step_id: None,
                            focus_id: Some(self.focus_id.clone()),
                            metadata: None,
                        }
                    };
                    let _ = self.write_focus_run_record("failed").await;
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
                    });
                }

                // Other errors: cleanup state and propagate.
                if self.focus_run_id.is_some() {
                    let _ = self.write_focus_run_record("failed").await;
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

    async fn execute_full_inner(&mut self) -> Result<RunResult, LifecycleError> {
        self.load().await?;
        self.authorize().await?;
        self.initialize().await?;

        if let Some(early_result) = self.execute().await? {
            let status = early_result.status.clone();
            self.cleanup(&status).await;
            return Ok(early_result);
        }

        // Capture step outputs while task_track is still live.
        // cleanup() later releases task_track, so extraction input
        // must be captured before output() completes.
        let step_outputs: Vec<(String, String)> = self
            .task_track
            .as_ref()
            .map(|tt| {
                tt.steps()
                    .iter()
                    .map(|s| (s.step_id.clone(), s.content.clone()))
                    .collect()
            })
            .unwrap_or_default();

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
        let candidates =
            crate::conductor::extract::extract_candidates(&step_outputs, &excluded_fields, &ollama)
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
            });
        }

        // No candidates -- output already saved. cleanup("complete") overwrites
        // 'awaiting_feedback' to 'complete' and purges snapshots.
        self.cleanup("complete").await;
        Ok(output_result)
    }
}

// ---------------------------------------------------------------------------
// demote_interrupted_runs
// ---------------------------------------------------------------------------

/// Update stale in-progress focus_runs to 'paused' status.
/// Called at startup to recover from sessions interrupted by crash or restart.
/// Returns the number of rows updated.
/// Python oracle: demote_interrupted_runs() standalone function.
pub async fn demote_interrupted_runs(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
) -> Result<u64, LifecycleError> {
    let threshold_minutes: i64 = std::env::var("QR_INTERRUPT_THRESHOLD_MINUTES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(5);

    let mut conn = open_outputs_db(user_id, persona_id, key_hex).await?;

    let result = sqlx::query(
        "UPDATE focus_runs SET status = 'paused'
         WHERE status IN ('running', 'initializing')
         AND started_at < datetime('now', ? || ' minutes')",
    )
    .bind(format!("-{threshold_minutes}"))
    .execute(&mut conn)
    .await?;

    Ok(result.rows_affected())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conductor::types::TaskStep;

    // -------------------------------------------------------------------------
    // parse_focus_definition
    // -------------------------------------------------------------------------

    fn minimal_raw() -> RawFocusFile {
        RawFocusFile {
            id: Some("quick-ask".to_owned()),
            display_name: Some("Quick Ask".to_owned()),
            description: Some("Fast single-step query".to_owned()),
            version: Some(serde_yaml::Value::String("1.0".to_owned())),
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
        assert_eq!(def.max_routing_tier, 1);
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
        assert!(!trigger.is_active(
            anchor,
            anchor + Duration::hours(-4) - Duration::seconds(1)
        ));
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
                step_type: None,
                output_var: Some("result".to_owned()),
                prompt_template: Some("Hello {user_input}".to_owned()),
                field_requirements: None,
                options_override: None,
            },
        );
        let mut raw = minimal_raw();
        raw.guides = Some(vec!["custom-guide".to_owned()]);
        raw.steps = Some(steps_map);

        let def = parse_focus_definition(raw).unwrap();
        assert_eq!(def.steps.len(), 1);
        assert_eq!(def.steps[0].guide_id, "custom-guide");
        assert_eq!(def.steps[0].step_id, "step_a");
        assert_eq!(def.steps[0].output_var.as_deref(), Some("result"));
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
                step_type: None,
                output_var: None,
                prompt_template: None,
                field_requirements: None,
                options_override: None,
            },
        );
        let mut raw = minimal_raw();
        raw.guides = None;
        raw.steps = Some(steps_map);

        let def = parse_focus_definition(raw).unwrap();
        assert_eq!(def.steps[0].guide_id, "quick-ask-guide");
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
            step_id: "s".to_owned(),
            output_var: None,
            content: "x".to_owned(),
            sensitivity_severity: ceiling,
            routing_tier_used: 1,
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
        assert_eq!(entry.step_id, "initialize");
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
}
