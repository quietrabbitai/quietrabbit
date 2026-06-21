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
// Privacy gateway (migration scaffold):
//   FocusRun holds Option<PrivacyGateway<NoopLogger>> for this migration phase.
//   NoopLogger always succeeds; gates enforce policy without persisting audit events.
//   Concrete logger wired in Layer 8 when disclosure_log DB writes are threaded through.
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
use crate::conductor::privacy::{logger::NoopLogger, PrivacyGateway};
use crate::conductor::tokens::{validate_step, FieldRequirement, StepDefinition, StepType};
use crate::conductor::types::{
    PersonalContextManifest, PersonalTrack, SharedStateTrack, TaskTrack,
};
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
    version: Option<serde_yaml::Value>,          // YAML may parse "1.0" as Number; stringify
    max_routing_tier: Option<u8>,
    output_types: Option<Vec<String>>,            // conductor-brief (preferred)
    output_type: Option<String>,                  // legacy single value — fallback only
    guides: Option<Vec<String>>,                  // focus-level; first entry = step default
    suggest_in_focuses: Option<Vec<String>>,
    multi_source_validation: Option<bool>,
    steps: Option<IndexMap<String, RawStep>>,     // IndexMap preserves YAML dict order
    #[allow(dead_code)] // D6-343: parsed for forward compat, not wired in Release 1.
    brief: Option<serde_yaml::Value>,
}

#[derive(Deserialize)]
struct RawStep {
    display_name: Option<String>,
    guide_id: Option<String>,                     // overrides focus-level default if present
    task_type: Option<String>,
    routing_tier: Option<u8>,
    step_type: Option<String>,
    output_var: Option<String>,
    prompt_template: Option<String>,
    field_requirements: Option<Vec<RawFieldRequirement>>,
    options_override: Option<serde_yaml::Value>,  // YAML map -> HashMap<String, json::Value>
}

#[derive(Deserialize)]
struct RawFieldRequirement {
    name: String,
    scope: String,
}

// ---------------------------------------------------------------------------
// parse_focus_definition
// ---------------------------------------------------------------------------

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
                guide_id: raw_step.guide_id.unwrap_or_else(|| default_guide_id.clone()),
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
    })
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
#[derive(Debug, Clone, Serialize)]
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
    let conn = connect_options_encrypted(&path, key_hex).connect().await?;
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
/// privacy_gateway: PrivacyGateway<NoopLogger> — migration scaffold.
///   NoopLogger always succeeds; gates enforce all policy without DB writes.
///   Concrete logger wired in Layer 8 when disclosure_log persistence is ready.
pub struct FocusRun {
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

    // Tauri AppHandle for push events — None in tests, Some in production (D6-345)
    pub app_handle: Option<tauri::AppHandle<tauri::Wry>>,

    // State populated during lifecycle phases (all Option — None until populated)
    pub focus_run_id: Option<String>,
    pub focus_def: Option<FocusDefinition>,
    pub personal_track: Option<PersonalTrack>,
    pub task_track: Option<TaskTrack>,
    pub shared_state: Option<SharedStateTrack>,
    pub failure_handler: Option<FailureHandler>,
    pub privacy_gateway: Option<PrivacyGateway<NoopLogger>>,

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
}

impl FocusRun {
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
        let focus_file = self.find_focus_file()?;
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

        self.focus_def = Some(focus_def);
        Ok(())
    }

    fn find_focus_file(&self) -> Result<PathBuf, LifecycleError> {
        // Two search locations. In Tauri production, focuses are embedded via
        // Tauri resources config (bundling TBD). Until bundled, resolve relative
        // to CWD (dev workflow — matches Python oracle's repo-relative path).
        let data_root = crate::providers::utils::get_data_root();
        let filename = format!("{}.focus", self.focus_id);

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
        Err(LifecycleError::FocusNotFound(self.focus_id.clone()))
    }

    // =========================================================================
    // Phase 2 — AUTHORIZE
    // =========================================================================

    /// Verify tier permissions and key; create focus_run record at status=initializing.
    /// Python oracle: FocusRun.authorize()
    pub async fn authorize(&mut self) -> Result<(), LifecycleError> {
        let _ = self.focus_def.as_ref()
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
        self.privacy_gateway = Some(PrivacyGateway::new(NoopLogger));
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
        use crate::persistence::personal_store::load_personal_track;

        let key_hex = self.key_hex.as_deref().unwrap_or("");
        let mut track = load_personal_track(&self.user_id, &self.persona_id, key_hex)
            .await
            .map_err(|e| LifecycleError::PersonalStore(e.to_string()))?;

        let focus_def = self.focus_def.as_ref()
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
            self.personal_track.as_ref().map(|t| t.is_sealed()).unwrap_or(false),
            "execute() requires a sealed PersonalTrack"
        );
        assert!(self.task_track.is_some(), "execute() requires task_track");
        assert!(self.shared_state.is_some(), "execute() requires shared_state");
        assert!(self.focus_def.is_some(), "execute() requires focus_def");
        assert!(self.focus_run_id.is_some(), "execute() requires focus_run_id");
        assert!(self.failure_handler.is_some(), "execute() requires failure_handler");
        assert!(self.privacy_gateway.is_some(), "execute() requires privacy_gateway");

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
            step.step_id, execution_tier, abstraction_tier,
            raw_abstraction, self._focus_privacy_tier,
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
            next_execution_tier,
            retry_count: 0,
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

        let final_content = self
            .task_track
            .as_ref()
            .unwrap()
            .last_output()
            .unwrap_or("")
            .to_owned();

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
        let Some(focus_run_id) = &self.focus_run_id else { return };
        let key_hex = self.key_hex.as_deref().unwrap_or("");
        if key_hex.is_empty() { return }
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
                self.focus_id, focus_run_id,
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
                    LifecycleError::TaxonomyIntegrity(msg) => Some(ConductorError::TaxonomyIntegrity {
                        plain_language: msg.clone(),
                    }),
                    LifecycleError::DatabaseMigration(msg) => Some(ConductorError::DatabaseMigration {
                        plain_language: msg.clone(),
                    }),
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

        let result = self.output().await?;
        self.cleanup("complete").await;
        Ok(result)
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
    }

    #[test]
    fn parse_output_type_fallback_chain() {
        let mut r = minimal_raw();
        r.output_types = Some(vec!["research_report".to_owned()]);
        r.output_type = Some("ignored".to_owned());
        assert_eq!(parse_focus_definition(r).unwrap().output_type, "research_report");

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
                display_name: None, guide_id: None, task_type: None,
                routing_tier: None, step_type: None, output_var: None,
                prompt_template: None, field_requirements: None, options_override: None,
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
            "u".to_owned(), "p".to_owned(), "f".to_owned(),
            scheduler, "".to_owned(), false, None, None, false, None,
        );
        let mut tt = TaskTrack::new();
        tt.add_step(TaskStep {
            step_id: "s".to_owned(), output_var: None,
            content: "x".to_owned(), sensitivity_severity: ceiling,
            routing_tier_used: 1,
        });
        run.task_track = Some(tt);
        run
    }

    #[test]
    fn output_sensitivity_general()   { assert_eq!(run_for_sensitivity(1).output_sensitivity(), "general");   }
    #[test]
    fn output_sensitivity_personal()  { assert_eq!(run_for_sensitivity(2).output_sensitivity(), "personal");  }
    #[test]
    fn output_sensitivity_medical()   { assert_eq!(run_for_sensitivity(3).output_sensitivity(), "medical");   }
    #[test]
    fn output_sensitivity_financial() { assert_eq!(run_for_sensitivity(4).output_sensitivity(), "financial"); }
    #[test]
    fn output_sensitivity_out_of_range_defaults_general() {
        assert_eq!(run_for_sensitivity(99).output_sensitivity(), "general");
    }
    #[test]
    fn output_sensitivity_no_track_defaults_general() {
        let scheduler = Arc::new(ConductorScheduler::new());
        let run = FocusRun::new(
            "u".to_owned(), "p".to_owned(), "f".to_owned(),
            scheduler, "".to_owned(), false, None, None, false, None,
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
}
