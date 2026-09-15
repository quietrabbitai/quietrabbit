// src-tauri/src/persistence/provider_store.rs
//
// providers CRUD for shared.db (unencrypted) -- items.id=427/428
// (PROVIDER_REGISTRY_AND_TIER_MODEL_SPEC.md Part 2), generalizing
// tier3_providers (items.id=202) into a flag-based table spanning every
// tier. shared_013.sql carries tier3_providers' 4 real seeded rows across
// and drops the old table -- this module is fully repointed at the new
// shape, not a parallel path. See shared_013.sql's own providers header
// for full column-by-column rationale; not re-derived here.
//
// CORE DESIGN RULE (Part 1): no tier column exists here and none should be
// added back. Eligibility/routing reads a decided flag column instead
// (is_local, is_anonymous, retains_data, trains_on_data_by_default,
// qr_internal_eligible, risk_rating, privacy_guardian_default_level).
// Tier labels are a display-layer-only concern computed from provider_type
// by callers that need one (e.g. commands::tier3_pane::lane_str) -- never
// read back into this module or into any decision logic.
//
// Backs TIER3_ACCESS_MODEL.md's selector screen (State 3, decisions.id=681)
// -- list_active_providers() is that screen's primary read path.
//
// QUERY STYLE: runtime sqlx::query() only -- no query!() macros, matching
// every other store in this module (persona_store.rs, focus_settings_store.rs).
// shared.db is unencrypted -- no PRAGMA key required.
//
// CONNECTION MODEL: pooled (items.id=483), matching persona_store.rs --
// callers pass a &sqlx::SqlitePool in; every fn here does pool.acquire().
//
// WRITE ACCESS: this module provides full CRUD (create/update/deactivate),
// but per decisions.id=710(b) the curated list is release-bundled, not
// user-editable at runtime -- no IPC command surface exposes these writes
// to the frontend. Writes are for release-time seeding (a future seed
// script/migration) and any future Chat-PM-directed catalog maintenance,
// not end-user action. Flagged here so a future reader doesn't assume a
// missing write-path IPC command is an oversight.

use sqlx::Row;
use thiserror::Error;

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum ProviderStoreError {
    #[error("Database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("Provider '{0}' already exists")]
    AlreadyExists(String),
    #[error("Provider '{0}' not found")]
    NotFound(String),
    #[error("Validation error: {0}")]
    Validation(String),
}

// ---------------------------------------------------------------------------
// Data types
// ---------------------------------------------------------------------------

/// mode: 'local' added for items.id=427 -- 'api' remains reserved per
/// shared_001.sql's original tier3_providers header (see shared_013.sql for
/// the carry-forward note); no Api-specific fields exist yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderMode {
    EmbeddedWeb,
    Api,
    Local,
}

impl ProviderMode {
    fn as_str(self) -> &'static str {
        match self {
            ProviderMode::EmbeddedWeb => "embedded_web",
            ProviderMode::Api => "api",
            ProviderMode::Local => "local",
        }
    }

    fn from_str(s: &str) -> Result<Self, ProviderStoreError> {
        match s {
            "embedded_web" => Ok(ProviderMode::EmbeddedWeb),
            "api" => Ok(ProviderMode::Api),
            "local" => Ok(ProviderMode::Local),
            other => Err(ProviderStoreError::Validation(format!(
                "mode must be 'embedded_web', 'api', or 'local', got '{other}' -- schema \
                 CHECK should have rejected this at write time."
            ))),
        }
    }
}

/// activation_status: 'active' | 'deprecated' only -- deliberately no
/// richer state machine, per shared_001.sql's original tier3_providers
/// header (decisions.id=710(b): release-bundled, not runtime-activated).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ActivationStatus {
    Active,
    Deprecated,
}

impl ActivationStatus {
    fn as_str(self) -> &'static str {
        match self {
            ActivationStatus::Active => "active",
            ActivationStatus::Deprecated => "deprecated",
        }
    }

    fn from_str(s: &str) -> Result<Self, ProviderStoreError> {
        match s {
            "active" => Ok(ActivationStatus::Active),
            "deprecated" => Ok(ActivationStatus::Deprecated),
            other => Err(ProviderStoreError::Validation(format!(
                "activation_status must be 'active' or 'deprecated', got '{other}' \
                 -- schema CHECK should have rejected this at write time."
            ))),
        }
    }
}

/// privacy_guardian_default_level: deliberately parallel to ReviewTier's
/// own three values (conductor/privacy/types.rs), same reasoning
/// risk_rating already established (shared_012.sql).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, specta::Type)]
#[serde(rename_all = "lowercase")]
pub enum PrivacyGuardianDefaultLevel {
    Low,
    Medium,
    High,
}

impl PrivacyGuardianDefaultLevel {
    fn as_str(self) -> &'static str {
        match self {
            PrivacyGuardianDefaultLevel::Low => "low",
            PrivacyGuardianDefaultLevel::Medium => "medium",
            PrivacyGuardianDefaultLevel::High => "high",
        }
    }

    fn from_str(s: &str) -> Result<Self, ProviderStoreError> {
        match s {
            "low" => Ok(PrivacyGuardianDefaultLevel::Low),
            "medium" => Ok(PrivacyGuardianDefaultLevel::Medium),
            "high" => Ok(PrivacyGuardianDefaultLevel::High),
            other => Err(ProviderStoreError::Validation(format!(
                "privacy_guardian_default_level must be 'low', 'medium', or 'high', got '{other}' \
                 -- schema CHECK should have rejected this at write time."
            ))),
        }
    }
}

/// items.id=465 (shared_016.sql): whether a provider's privacy posture is
/// backed by contractual language (DPA/Services Agreement) or is merely
/// descriptive policy prose with no contractual commitment. Human-curated
/// only, same as privacy_guardian_default_level -- never derived from
/// documentation_gate's freeform research text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, specta::Type)]
#[serde(rename_all = "snake_case")]
pub enum PrivacyCommitmentBasis {
    Contractual,
    PolicyOnly,
}

impl PrivacyCommitmentBasis {
    fn as_str(self) -> &'static str {
        match self {
            PrivacyCommitmentBasis::Contractual => "contractual",
            PrivacyCommitmentBasis::PolicyOnly => "policy_only",
        }
    }

    fn from_str(s: &str) -> Result<Self, ProviderStoreError> {
        match s {
            "contractual" => Ok(PrivacyCommitmentBasis::Contractual),
            "policy_only" => Ok(PrivacyCommitmentBasis::PolicyOnly),
            other => Err(ProviderStoreError::Validation(format!(
                "privacy_commitment_basis must be 'contractual' or 'policy_only', got '{other}' \
                 -- schema CHECK should have rejected this at write time."
            ))),
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Provider {
    pub id: String,
    pub display_name: String,
    /// Open vocabulary, no CHECK -- mechanical integration shape
    /// ('local_model' | 'cloud_inference_api' | 'split_screen_web' |
    /// 'external_service'), not a classification. Never branched on for
    /// eligibility -- only display grouping (see commands::tier3_pane).
    pub provider_type: String,
    pub mode: ProviderMode,
    pub launch_url: Option<String>,
    pub activation_status: ActivationStatus,
    /// Stored as JSON TEXT in DB. Default is empty object {}. Holds
    /// decisions.id=710(a)'s documentation-gate fields (ToS/retention
    /// citation, jurisdiction, contradictory-report notes) -- also this
    /// provider's selector-card retention-posture display source, per
    /// shared_001.sql's original CARD DISPLAY note.
    pub documentation_gate: serde_json::Value,
    /// Consumer-facing plain-language privacy summary (JSON) -- this
    /// provider's own "what this means for you" explainer content, distinct
    /// from documentation_gate's compliance/audit-trail research framing.
    /// NULL until curated.
    pub user_privacy_summary: Option<serde_json::Value>,
    pub last_reviewed_at: Option<String>,
    pub review_trigger_note: Option<String>,
    pub created_at: String,
    /// Runs on the user's own hardware. Decided at curation time, never
    /// derived from provider_type.
    pub is_local: bool,
    /// No login, no persistent identity.
    pub is_anonymous: bool,
    /// Reviewed yes/no -- not a formula over documentation_gate's
    /// retention-summary prose.
    pub retains_data: bool,
    /// The specific, high-stakes fact behind the OpenAI/Gemini
    /// documentation_gate caveats.
    pub trains_on_data_by_default: bool,
    pub login_required: bool,
    /// Part 3b's floor mechanism. Defaults false -- a provider must be
    /// explicitly marked eligible for QR-internal operations (Privacy
    /// Guardian's own evaluation, document ingestion/extraction).
    pub qr_internal_eligible: bool,
    pub privacy_guardian_default_level: Option<PrivacyGuardianDefaultLevel>,
    /// items.id=406 (decisions.id=753): live per-provider destination risk
    /// rating driving Privacy Guardian routing -- 1=Low, 2=Medium, 3=High.
    /// Deliberately its own column, not folded into documentation_gate's
    /// freeform display JSON -- see shared_012.sql's header for why.
    pub risk_rating: u8,
    /// Tier 1/1.5 rows only. Min RAM/VRAM class, expected tokens/sec on a
    /// reference hardware class -- objectively measurable, so JSON is an
    /// acceptable escape hatch here unlike the flag columns above.
    pub hardware_requirement: Option<serde_json::Value>,
    /// items.id=465 (shared_016.sql): whether QR itself recommends this
    /// provider, interpreted jointly with provider_type -- one
    /// recommendation slot per provider_type (cloud_inference_api /
    /// split_screen_web / external_service), NOT a cross-slot rankable
    /// field. DEFAULT false -- same unmarked-is-excluded convention as
    /// qr_internal_eligible.
    pub qr_recommended: bool,
    /// items.id=465 (shared_016.sql): human-curated only, NULL until
    /// assessed -- never derived from documentation_gate's freeform prose.
    pub privacy_commitment_basis: Option<PrivacyCommitmentBasis>,
    /// items.id=465 (shared_016.sql): throughput/latency class (JSON,
    /// nullable), decoupled from hardware_requirement -- any provider row
    /// can carry a performance_profile regardless of install footprint.
    pub performance_profile: Option<serde_json::Value>,
}

/// Input to create_provider(). A plain struct rather than 15+ positional
/// arguments -- the prior 7-argument signature already carried
/// #[allow(clippy::too_many_arguments)]; adding Part 2's 8 new decided-flag
/// columns on top of that would make positional bool/string arguments a
/// real transposition hazard, not just a style concern.
pub struct NewProvider<'a> {
    pub id: &'a str,
    pub display_name: &'a str,
    pub provider_type: &'a str,
    pub mode: ProviderMode,
    pub launch_url: Option<&'a str>,
    pub login_required: bool,
    pub is_local: bool,
    pub is_anonymous: bool,
    pub retains_data: bool,
    pub trains_on_data_by_default: bool,
    pub qr_internal_eligible: bool,
    pub privacy_guardian_default_level: Option<PrivacyGuardianDefaultLevel>,
    pub risk_rating: u8,
    pub hardware_requirement: Option<serde_json::Value>,
    pub documentation_gate: &'a serde_json::Value,
    /// items.id=465: defaults false/None at every existing call site
    /// (release-time seeding still curates these via a migration's own
    /// UPDATE, not through create_provider -- same precedent shared_014.sql
    /// already set for qr_internal_eligible).
    pub qr_recommended: bool,
    pub privacy_commitment_basis: Option<PrivacyCommitmentBasis>,
    pub performance_profile: Option<serde_json::Value>,
}

// ---------------------------------------------------------------------------
// DB opener (shared.db — unencrypted)
// ---------------------------------------------------------------------------

const SELECT_COLUMNS: &str = "id, display_name, provider_type, mode, launch_url,
                activation_status, documentation_gate, last_reviewed_at,
                review_trigger_note, created_at, is_local, is_anonymous,
                retains_data, trains_on_data_by_default, login_required,
                qr_internal_eligible, privacy_guardian_default_level,
                risk_rating, hardware_requirement, user_privacy_summary,
                qr_recommended, privacy_commitment_basis, performance_profile";

// ---------------------------------------------------------------------------
// Row extraction
// ---------------------------------------------------------------------------

fn row_to_provider(row: &sqlx::sqlite::SqliteRow) -> Result<Provider, ProviderStoreError> {
    let id: String = row.try_get("id").map_err(ProviderStoreError::Database)?;
    let mode_raw: String = row.try_get("mode").map_err(ProviderStoreError::Database)?;
    let login_required_raw: i64 = row
        .try_get("login_required")
        .map_err(ProviderStoreError::Database)?;
    let activation_status_raw: String = row
        .try_get("activation_status")
        .map_err(ProviderStoreError::Database)?;
    let doc_gate_raw: String = row
        .try_get("documentation_gate")
        .map_err(ProviderStoreError::Database)?;
    let is_local_raw: i64 = row
        .try_get("is_local")
        .map_err(ProviderStoreError::Database)?;
    let is_anonymous_raw: i64 = row
        .try_get("is_anonymous")
        .map_err(ProviderStoreError::Database)?;
    let retains_data_raw: i64 = row
        .try_get("retains_data")
        .map_err(ProviderStoreError::Database)?;
    let trains_raw: i64 = row
        .try_get("trains_on_data_by_default")
        .map_err(ProviderStoreError::Database)?;
    let qr_internal_raw: i64 = row
        .try_get("qr_internal_eligible")
        .map_err(ProviderStoreError::Database)?;
    let pg_level_raw: Option<String> = row
        .try_get("privacy_guardian_default_level")
        .map_err(ProviderStoreError::Database)?;
    let hardware_req_raw: Option<String> = row
        .try_get("hardware_requirement")
        .map_err(ProviderStoreError::Database)?;
    let user_privacy_summary_raw: Option<String> = row
        .try_get("user_privacy_summary")
        .map_err(ProviderStoreError::Database)?;
    let qr_recommended_raw: i64 = row
        .try_get("qr_recommended")
        .map_err(ProviderStoreError::Database)?;
    let privacy_commitment_basis_raw: Option<String> = row
        .try_get("privacy_commitment_basis")
        .map_err(ProviderStoreError::Database)?;
    let performance_profile_raw: Option<String> = row
        .try_get("performance_profile")
        .map_err(ProviderStoreError::Database)?;

    let documentation_gate: serde_json::Value =
        serde_json::from_str(&doc_gate_raw).unwrap_or_else(|e| {
            log::warn!(
                "provider '{id}' documentation_gate failed to parse as JSON, \
                 defaulting to empty object: {e}"
            );
            serde_json::Value::Object(serde_json::Map::new())
        });

    let hardware_requirement = hardware_req_raw.and_then(|raw| {
        serde_json::from_str(&raw)
            .map_err(|e| {
                log::warn!(
                    "provider '{id}' hardware_requirement failed to parse as JSON, \
                     dropping: {e}"
                );
            })
            .ok()
    });

    let privacy_guardian_default_level = pg_level_raw
        .as_deref()
        .map(PrivacyGuardianDefaultLevel::from_str)
        .transpose()?;

    let user_privacy_summary = user_privacy_summary_raw.and_then(|raw| {
        serde_json::from_str(&raw)
            .map_err(|e| {
                log::warn!(
                    "provider '{id}' user_privacy_summary failed to parse as JSON, \
                     dropping: {e}"
                );
            })
            .ok()
    });

    let privacy_commitment_basis = privacy_commitment_basis_raw
        .as_deref()
        .map(PrivacyCommitmentBasis::from_str)
        .transpose()?;

    let performance_profile = performance_profile_raw.and_then(|raw| {
        serde_json::from_str(&raw)
            .map_err(|e| {
                log::warn!(
                    "provider '{id}' performance_profile failed to parse as JSON, \
                     dropping: {e}"
                );
            })
            .ok()
    });

    Ok(Provider {
        id,
        display_name: row
            .try_get("display_name")
            .map_err(ProviderStoreError::Database)?,
        provider_type: row
            .try_get("provider_type")
            .map_err(ProviderStoreError::Database)?,
        mode: ProviderMode::from_str(&mode_raw)?,
        launch_url: row
            .try_get("launch_url")
            .map_err(ProviderStoreError::Database)?,
        activation_status: ActivationStatus::from_str(&activation_status_raw)?,
        documentation_gate,
        user_privacy_summary,
        last_reviewed_at: row
            .try_get("last_reviewed_at")
            .map_err(ProviderStoreError::Database)?,
        review_trigger_note: row
            .try_get("review_trigger_note")
            .map_err(ProviderStoreError::Database)?,
        created_at: row
            .try_get("created_at")
            .map_err(ProviderStoreError::Database)?,
        is_local: is_local_raw != 0,
        is_anonymous: is_anonymous_raw != 0,
        retains_data: retains_data_raw != 0,
        trains_on_data_by_default: trains_raw != 0,
        login_required: login_required_raw != 0,
        qr_internal_eligible: qr_internal_raw != 0,
        privacy_guardian_default_level,
        risk_rating: {
            let raw: i64 = row
                .try_get("risk_rating")
                .map_err(ProviderStoreError::Database)?;
            raw as u8
        },
        hardware_requirement,
        qr_recommended: qr_recommended_raw != 0,
        privacy_commitment_basis,
        performance_profile,
    })
}

// ---------------------------------------------------------------------------
// Constraint error classifier
// ---------------------------------------------------------------------------

/// Mirrors persona_store.rs's classify_constraint_error: same numeric-code
/// and message-substring double-check rationale, for sqlx-version and
/// SQLite-build portability. Not re-derived here.
fn classify_constraint_error(provider_id: &str, e: sqlx::Error) -> ProviderStoreError {
    if let Some(db_err) = e.as_database_error() {
        let code = db_err.code().unwrap_or_default();
        let msg = db_err.message().to_lowercase();
        let is_unique = matches!(code.as_ref(), "19" | "1555" | "2067")
            || msg.contains("unique constraint failed");
        if is_unique {
            return ProviderStoreError::AlreadyExists(provider_id.to_owned());
        }
    }
    ProviderStoreError::Database(e)
}

// ---------------------------------------------------------------------------
// Read operations
// ---------------------------------------------------------------------------

/// Fetch a provider by ID. Returns None if not found. No activation_status
/// filter -- callers wanting only 'active' rows should use
/// list_active_providers() or filter explicitly; this is the raw lookup.
pub async fn get_provider(
    pool: &sqlx::SqlitePool,
    provider_id: &str,
) -> Result<Option<Provider>, ProviderStoreError> {
    let mut conn = pool.acquire().await?;

    let sql = format!("SELECT {SELECT_COLUMNS} FROM providers WHERE id = ?");
    let row = sqlx::query(&sql)
        .bind(provider_id)
        .fetch_optional(&mut *conn)
        .await?;

    match row {
        None => Ok(None),
        Some(r) => Ok(Some(row_to_provider(&r)?)),
    }
}

/// items.id=406 (decisions.id=753): the routing-determinant read path for
/// Privacy Guardian gate3 -- the worst-case (MAX) risk rating across
/// whatever destinations are currently active/selected, so a copy-time
/// review that covers several simultaneously-open providers is scored
/// against the riskiest one, not an arbitrary single pick. `None` when
/// `provider_ids` is empty (no known destination yet -- callers should
/// treat this the same as "no rating available", i.e. fall back to
/// whatever conservative default they'd otherwise use).
///
/// items.id=427: repointed at providers -- signature and behavior
/// otherwise unchanged, this is a live Privacy Guardian consumer.
pub async fn max_risk_rating_for_providers(
    pool: &sqlx::SqlitePool,
    provider_ids: &[String],
) -> Result<Option<u8>, ProviderStoreError> {
    if provider_ids.is_empty() {
        return Ok(None);
    }

    let mut conn = pool.acquire().await?;

    // Bound to (small) actual provider IDs, not user-supplied text -- an
    // IN(...) list built from a fixed, checked-length local set is standard
    // sqlx practice, not user-controlled SQL. Placeholders are still bound
    // by position, never interpolated, so this carries no injection risk.
    let placeholders = provider_ids
        .iter()
        .map(|_| "?")
        .collect::<Vec<_>>()
        .join(", ");
    let sql =
        format!("SELECT MAX(risk_rating) as max_risk FROM providers WHERE id IN ({placeholders})");

    let mut query = sqlx::query(&sql);
    for id in provider_ids {
        query = query.bind(id);
    }

    let row = query.fetch_one(&mut *conn).await?;
    let max_risk: Option<i64> = row
        .try_get("max_risk")
        .map_err(ProviderStoreError::Database)?;
    Ok(max_risk.map(|r| r as u8))
}

/// The selector screen's primary read path (TIER3_ACCESS_MODEL.md State 3):
/// all 'active' providers, ordered provider_type then display_name so a
/// caller can group the result for display without a second query. No
/// stability contract beyond "grouped and deterministic" -- provider_type
/// replaces the old tier-based ordering (items.id=427: no tier column).
pub async fn list_active_providers(
    pool: &sqlx::SqlitePool,
) -> Result<Vec<Provider>, ProviderStoreError> {
    let mut conn = pool.acquire().await?;

    let sql = format!(
        "SELECT {SELECT_COLUMNS}
         FROM providers
         WHERE activation_status = 'active'
         ORDER BY provider_type ASC, display_name ASC"
    );
    let rows = sqlx::query(&sql).fetch_all(&mut *conn).await?;

    let mut providers = Vec::new();
    for r in rows {
        providers.push(row_to_provider(&r)?);
    }
    Ok(providers)
}

/// items.id=430/432: active providers of a given `provider_type` --
/// the flag-based replacement for hardcoded provider-name arrays
/// (commands/system.rs's retired TIER2_PROVIDERS const, conductor/
/// lifecycle.rs's Tier-1.5 candidate set). `provider_type` is a decided,
/// open-vocabulary column (Part 2), not a tier label -- filtering on it is
/// exactly the flag-based eligibility this table exists to provide (core
/// rule 2), not a reintroduction of the old hardcoding problem.
pub async fn list_providers_by_type(
    pool: &sqlx::SqlitePool,
    provider_type: &str,
) -> Result<Vec<Provider>, ProviderStoreError> {
    let mut conn = pool.acquire().await?;

    let sql = format!(
        "SELECT {SELECT_COLUMNS}
         FROM providers
         WHERE activation_status = 'active' AND provider_type = ?
         ORDER BY display_name ASC"
    );
    let rows = sqlx::query(&sql)
        .bind(provider_type)
        .fetch_all(&mut *conn)
        .await?;

    let mut providers = Vec::new();
    for r in rows {
        providers.push(row_to_provider(&r)?);
    }
    Ok(providers)
}

/// All providers regardless of activation_status, for admin/maintenance
/// views (e.g. a future Chat-PM-facing catalog-review surface) -- NOT the
/// selector screen's path, which must use list_active_providers().
pub async fn list_all_providers(
    pool: &sqlx::SqlitePool,
) -> Result<Vec<Provider>, ProviderStoreError> {
    let mut conn = pool.acquire().await?;

    let sql = format!(
        "SELECT {SELECT_COLUMNS}
         FROM providers
         ORDER BY provider_type ASC, display_name ASC"
    );
    let rows = sqlx::query(&sql).fetch_all(&mut *conn).await?;

    let mut providers = Vec::new();
    for r in rows {
        providers.push(row_to_provider(&r)?);
    }
    Ok(providers)
}

// ---------------------------------------------------------------------------
// provider_models (items.id=465, shared_016.sql)
// ---------------------------------------------------------------------------

/// One catalog model entry for a provider. `id` is literally
/// "provider_id:model_id" -- the same opaque string
/// conductor/executor.rs's select_model()/get_context_window() already
/// pass around (e.g. "groq:llama-3.1-8b-instant"), so callers never split
/// or reassemble it.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ProviderModel {
    pub id: String,
    pub provider_id: String,
    pub model_id: String,
    pub context_window_tokens: u32,
    pub is_default: bool,
    pub created_at: String,
}

fn row_to_provider_model(
    row: &sqlx::sqlite::SqliteRow,
) -> Result<ProviderModel, ProviderStoreError> {
    let context_window_tokens: i64 = row
        .try_get("context_window_tokens")
        .map_err(ProviderStoreError::Database)?;
    let is_default: i64 = row
        .try_get("is_default")
        .map_err(ProviderStoreError::Database)?;

    Ok(ProviderModel {
        id: row.try_get("id").map_err(ProviderStoreError::Database)?,
        provider_id: row
            .try_get("provider_id")
            .map_err(ProviderStoreError::Database)?,
        model_id: row
            .try_get("model_id")
            .map_err(ProviderStoreError::Database)?,
        context_window_tokens: context_window_tokens as u32,
        is_default: is_default != 0,
        created_at: row
            .try_get("created_at")
            .map_err(ProviderStoreError::Database)?,
    })
}

/// The model conductor/executor.rs's select_model() resolves for a Tier 2
/// provider -- replaces the hardcoded `match tier2_provider { Some("mistral")
/// => ..., Some("groq") => ... }` literal-string dispatch. `None` when
/// `provider_id` has no default row in provider_models (an unknown provider,
/// or a known one not yet curated with a model) -- callers must treat that
/// as a real failure (ConductorError::UnknownProvider in executor.rs), not
/// silently fall back to any other provider.
pub async fn get_default_model(
    pool: &sqlx::SqlitePool,
    provider_id: &str,
) -> Result<Option<ProviderModel>, ProviderStoreError> {
    let mut conn = pool.acquire().await?;

    let row = sqlx::query(
        "SELECT id, provider_id, model_id, context_window_tokens, is_default, created_at
         FROM provider_models
         WHERE provider_id = ? AND is_default = 1",
    )
    .bind(provider_id)
    .fetch_optional(&mut *conn)
    .await?;

    match row {
        None => Ok(None),
        Some(r) => Ok(Some(row_to_provider_model(&r)?)),
    }
}

/// Look up a catalog model by its real `provider_id`/`model_id` columns --
/// used by conductor/executor.rs's get_context_window() at Tier 2 instead of
/// reconstructing the composite `id` PK from those same two fields
/// (decisions.id=813: an id's internal structure is never treated as data,
/// in either direction -- parsing it apart or rebuilding it to use as a key).
pub async fn get_model_by_provider_and_model_id(
    pool: &sqlx::SqlitePool,
    provider_id: &str,
    model_id: &str,
) -> Result<Option<ProviderModel>, ProviderStoreError> {
    let mut conn = pool.acquire().await?;

    let row = sqlx::query(
        "SELECT id, provider_id, model_id, context_window_tokens, is_default, created_at
         FROM provider_models
         WHERE provider_id = ? AND model_id = ?",
    )
    .bind(provider_id)
    .bind(model_id)
    .fetch_optional(&mut *conn)
    .await?;

    match row {
        None => Ok(None),
        Some(r) => Ok(Some(row_to_provider_model(&r)?)),
    }
}

// ---------------------------------------------------------------------------
// Write operations
// ---------------------------------------------------------------------------

/// Create a new provider catalog row. Release-time/catalog-maintenance use
/// only (see module header WRITE ACCESS note) -- not exposed via IPC.
/// Returns Err(AlreadyExists) if provider_id already exists.
pub async fn create_provider(
    pool: &sqlx::SqlitePool,
    new: NewProvider<'_>,
) -> Result<Provider, ProviderStoreError> {
    if new.mode == ProviderMode::EmbeddedWeb && new.launch_url.is_none() {
        return Err(ProviderStoreError::Validation(
            "launch_url is required when mode='embedded_web' -- the pane has \
             nothing to point CEF at otherwise."
                .to_owned(),
        ));
    }

    let created_at = crate::providers::utils::now();
    let doc_gate_str = serde_json::to_string(new.documentation_gate).map_err(|e| {
        ProviderStoreError::Validation(format!("documentation_gate not valid JSON: {e}"))
    })?;
    let hardware_req_str = new
        .hardware_requirement
        .as_ref()
        .map(serde_json::to_string)
        .transpose()
        .map_err(|e| {
            ProviderStoreError::Validation(format!("hardware_requirement not valid JSON: {e}"))
        })?;
    let performance_profile_str = new
        .performance_profile
        .as_ref()
        .map(serde_json::to_string)
        .transpose()
        .map_err(|e| {
            ProviderStoreError::Validation(format!("performance_profile not valid JSON: {e}"))
        })?;
    let mut conn = pool.acquire().await?;

    sqlx::query(
        "INSERT INTO providers
         (id, display_name, provider_type, mode, launch_url, login_required,
          activation_status, documentation_gate, last_reviewed_at,
          review_trigger_note, created_at, is_local, is_anonymous,
          retains_data, trains_on_data_by_default, qr_internal_eligible,
          privacy_guardian_default_level, risk_rating, hardware_requirement,
          qr_recommended, privacy_commitment_basis, performance_profile)
         VALUES (?, ?, ?, ?, ?, ?, 'active', ?, NULL, NULL, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(new.id)
    .bind(new.display_name)
    .bind(new.provider_type)
    .bind(new.mode.as_str())
    .bind(new.launch_url)
    .bind(new.login_required as i64)
    .bind(&doc_gate_str)
    .bind(&created_at)
    .bind(new.is_local as i64)
    .bind(new.is_anonymous as i64)
    .bind(new.retains_data as i64)
    .bind(new.trains_on_data_by_default as i64)
    .bind(new.qr_internal_eligible as i64)
    .bind(new.privacy_guardian_default_level.map(|l| l.as_str()))
    .bind(new.risk_rating as i64)
    .bind(&hardware_req_str)
    .bind(new.qr_recommended as i64)
    .bind(new.privacy_commitment_basis.map(|b| b.as_str()))
    .bind(&performance_profile_str)
    .execute(&mut *conn)
    .await
    .map_err(|e| classify_constraint_error(new.id, e))?;

    Ok(Provider {
        id: new.id.to_owned(),
        display_name: new.display_name.to_owned(),
        provider_type: new.provider_type.to_owned(),
        mode: new.mode,
        launch_url: new.launch_url.map(|s| s.to_owned()),
        activation_status: ActivationStatus::Active,
        documentation_gate: new.documentation_gate.clone(),
        user_privacy_summary: None,
        last_reviewed_at: None,
        review_trigger_note: None,
        created_at,
        is_local: new.is_local,
        is_anonymous: new.is_anonymous,
        retains_data: new.retains_data,
        trains_on_data_by_default: new.trains_on_data_by_default,
        login_required: new.login_required,
        qr_internal_eligible: new.qr_internal_eligible,
        privacy_guardian_default_level: new.privacy_guardian_default_level,
        risk_rating: new.risk_rating,
        hardware_requirement: new.hardware_requirement,
        qr_recommended: new.qr_recommended,
        privacy_commitment_basis: new.privacy_commitment_basis,
        performance_profile: new.performance_profile,
    })
}

/// Set activation_status. The only status transition this table's own
/// lifecycle needs (decisions.id=710(b): release-bundled, no richer state
/// machine) -- 'deprecated' rows stay in the table for audit/history
/// rather than being deleted.
pub async fn set_activation_status(
    pool: &sqlx::SqlitePool,
    provider_id: &str,
    status: ActivationStatus,
) -> Result<(), ProviderStoreError> {
    let mut conn = pool.acquire().await?;

    let result = sqlx::query("UPDATE providers SET activation_status = ? WHERE id = ?")
        .bind(status.as_str())
        .bind(provider_id)
        .execute(&mut *conn)
        .await?;

    if result.rows_affected() == 0 {
        return Err(ProviderStoreError::NotFound(provider_id.to_owned()));
    }
    Ok(())
}

/// Record a review per decisions.id=710(c)'s event-triggered (not
/// calendar-fixed) monitoring cadence -- called when a real signal fires
/// (a ToS/retention-policy change, a credible contradictory report), not
/// on any schedule this module or its caller maintains. Updates
/// last_reviewed_at and review_trigger_note together, since a review
/// without a recorded trigger reason would defeat the audit-trail purpose
/// review_trigger_note exists for (mirrors focus_settings_store.rs's
/// record_friction_gate_decision validation shape: at least one
/// meaningful field required, not silently accepted empty).
pub async fn record_review(
    pool: &sqlx::SqlitePool,
    provider_id: &str,
    trigger_note: &str,
) -> Result<(), ProviderStoreError> {
    if trigger_note.trim().is_empty() {
        return Err(ProviderStoreError::Validation(
            "review_trigger_note must be non-empty -- a review record with no \
             stated trigger reason defeats the audit trail this field exists for."
                .to_owned(),
        ));
    }

    let reviewed_at = crate::providers::utils::now();
    let mut conn = pool.acquire().await?;

    let result = sqlx::query(
        "UPDATE providers
         SET last_reviewed_at = ?, review_trigger_note = ?
         WHERE id = ?",
    )
    .bind(&reviewed_at)
    .bind(trigger_note)
    .bind(provider_id)
    .execute(&mut *conn)
    .await?;

    if result.rows_affected() == 0 {
        return Err(ProviderStoreError::NotFound(provider_id.to_owned()));
    }
    Ok(())
}

/// Update documentation_gate content (e.g. after a review updates the
/// citation/jurisdiction fields). Does NOT touch last_reviewed_at/
/// review_trigger_note -- callers doing both should call record_review()
/// separately, keeping "what changed" and "why it was reviewed" as two
/// explicit calls rather than one that could silently update content
/// without a recorded trigger.
pub async fn update_documentation_gate(
    pool: &sqlx::SqlitePool,
    provider_id: &str,
    documentation_gate: &serde_json::Value,
) -> Result<(), ProviderStoreError> {
    let doc_gate_str = serde_json::to_string(documentation_gate).map_err(|e| {
        ProviderStoreError::Validation(format!("documentation_gate not valid JSON: {e}"))
    })?;
    let mut conn = pool.acquire().await?;

    let result = sqlx::query("UPDATE providers SET documentation_gate = ? WHERE id = ?")
        .bind(&doc_gate_str)
        .bind(provider_id)
        .execute(&mut *conn)
        .await?;

    if result.rows_affected() == 0 {
        return Err(ProviderStoreError::NotFound(provider_id.to_owned()));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::SqliteConnectOptions;
    use sqlx::{ConnectOptions, SqliteConnection};

    /// Mirrors migrations.rs's own make_test_conn (private to that module's
    /// test mod, so not reusable directly) -- same in-memory, unencrypted
    /// connection shape.
    async fn make_test_conn() -> SqliteConnection {
        SqliteConnectOptions::new()
            .filename(":memory:")
            .connect()
            .await
            .expect("in-memory connection failed")
    }

    /// items.id=427: asserts shared_013.sql's migration of tier3_providers'
    /// 4 real rows into providers actually preserves the live-consumer-
    /// critical values (risk_rating, login_required) and the new
    /// provider_type derivation -- not just eyeballed against the SQL.
    #[tokio::test]
    async fn migrated_providers_preserve_risk_rating_and_login_required() {
        let mut conn = make_test_conn().await;
        crate::persistence::migrations::run_migrations(&mut conn, "shared", None)
            .await
            .expect("run shared migrations");

        let duckai = get_provider_via_conn(&mut conn, "duckai").await;
        assert_eq!(duckai.risk_rating, 1);
        assert!(!duckai.login_required);
        assert_eq!(duckai.provider_type, "split_screen_web");
        assert!(duckai.is_anonymous);
        assert!(!duckai.retains_data);
        assert!(!duckai.trains_on_data_by_default);

        for id in ["claude", "chatgpt", "gemini"] {
            let p = get_provider_via_conn(&mut conn, id).await;
            assert_eq!(p.risk_rating, 3, "{id} must keep its High risk rating");
            assert!(p.login_required, "{id} must keep login_required=1");
            assert_eq!(p.provider_type, "external_service");
            assert!(!p.is_anonymous);
            assert!(
                p.retains_data,
                "{id} retains data per its documentation_gate"
            );
            assert!(
                p.trains_on_data_by_default,
                "{id} trains on data by default per its documentation_gate"
            );
        }
    }

    #[tokio::test]
    async fn tier3_providers_table_no_longer_exists_after_migration() {
        let mut conn = make_test_conn().await;
        crate::persistence::migrations::run_migrations(&mut conn, "shared", None)
            .await
            .expect("run shared migrations");

        let exists: Option<(String,)> = sqlx::query_as(
            "SELECT name FROM sqlite_master WHERE type='table' AND name='tier3_providers'",
        )
        .fetch_optional(&mut conn)
        .await
        .unwrap();
        assert!(
            exists.is_none(),
            "tier3_providers must be dropped once generalized into providers"
        );
    }

    async fn get_provider_via_conn(conn: &mut SqliteConnection, id: &str) -> Provider {
        let sql = format!("SELECT {SELECT_COLUMNS} FROM providers WHERE id = ?");
        let row = sqlx::query(&sql)
            .bind(id)
            .fetch_one(&mut *conn)
            .await
            .unwrap_or_else(|e| panic!("provider '{id}' must exist after migration: {e}"));
        row_to_provider(&row).unwrap_or_else(|e| panic!("provider '{id}' row shape: {e}"))
    }

    /// Exercises the real public API (list_active_providers,
    /// max_risk_rating_for_providers) against a real on-disk shared.db under
    /// a tempdir-backed QR_DATA_ROOT -- not just the migration SQL directly
    /// (the two tests above). This is the live Privacy Guardian consumer's
    /// actual call path. Pattern matches migrations.rs's own
    /// QR_DATA_ROOT-mutating tests (ENV_MUTEX serialization, save/restore).
    #[tokio::test]
    async fn list_active_providers_and_max_risk_rating_via_public_api() {
        let _lock = crate::test_support::ENV_MUTEX.lock().await;
        let saved_root = std::env::var("QR_DATA_ROOT").ok();
        let tempdir = tempfile::tempdir().expect("failed to create tempdir");
        std::env::set_var("QR_DATA_ROOT", tempdir.path());

        let result = crate::persistence::migrations::migrate_shared_db().await;

        let outcome = async {
            let pool = sqlx::SqlitePool::connect_with(
                crate::providers::utils::connect_options_unencrypted(
                    &crate::providers::utils::db_path_shared(),
                ),
            )
            .await?;
            let providers = list_active_providers(&pool).await?;
            let ids: Vec<&str> = providers.iter().map(|p| p.id.as_str()).collect();
            assert!(ids.contains(&"duckai"));
            assert!(ids.contains(&"claude"));
            assert!(ids.contains(&"chatgpt"));
            assert!(ids.contains(&"gemini"));

            let max_risk =
                max_risk_rating_for_providers(&pool, &["duckai".to_string(), "claude".to_string()])
                    .await?;
            assert_eq!(
                max_risk,
                Some(3),
                "MAX across a Low(duckai)+High(claude) selection must be High"
            );

            let max_risk_low_only =
                max_risk_rating_for_providers(&pool, &["duckai".to_string()]).await?;
            assert_eq!(max_risk_low_only, Some(1));

            Ok::<(), ProviderStoreError>(())
        }
        .await;

        if let Some(v) = saved_root {
            std::env::set_var("QR_DATA_ROOT", v);
        } else {
            std::env::remove_var("QR_DATA_ROOT");
        }

        result.expect("migrate_shared_db must succeed");
        outcome.expect("public API assertions must pass");
    }

    /// items.id=430/432: shared_014.sql seeds groq/mistral as
    /// provider_type='cloud_inference_api' rows -- confirms the migration
    /// landed the mechanically-known facts correctly. items.id=440 Part A
    /// (this session) replaced the original placeholder seed with real,
    /// sourced curation flags -- distinct per provider, no longer the
    /// shared conservative defaults the placeholder left them at.
    #[tokio::test]
    async fn groq_and_mistral_seeded_as_cloud_inference_api() {
        let mut conn = make_test_conn().await;
        crate::persistence::migrations::run_migrations(&mut conn, "shared", None)
            .await
            .expect("run shared migrations");

        for id in ["groq", "mistral"] {
            let p = get_provider_via_conn(&mut conn, id).await;
            assert_eq!(p.provider_type, "cloud_inference_api");
            assert_eq!(p.mode, ProviderMode::Api);
            assert!(!p.is_local, "{id} is not local (Tier 1.5)");
            assert!(
                p.login_required,
                "{id} requires login (Tier 1.5, non-anonymous)"
            );
            assert!(!p.is_anonymous, "{id} is not anonymous");
            assert!(!p.qr_internal_eligible);
        }

        let groq = get_provider_via_conn(&mut conn, "groq").await;
        assert!(
            !groq.retains_data,
            "groq: not retained beyond providing the service, per its DPA"
        );
        assert!(
            !groq.trains_on_data_by_default,
            "groq: not trained on by default, per its Services Agreement"
        );
        assert_eq!(groq.risk_rating, 1);
        assert_eq!(
            groq.privacy_guardian_default_level,
            Some(PrivacyGuardianDefaultLevel::Low)
        );

        let mistral = get_provider_via_conn(&mut conn, "mistral").await;
        assert!(
            mistral.retains_data,
            "mistral: 30-day abuse-monitoring log retained by default"
        );
        assert!(
            mistral.trains_on_data_by_default,
            "mistral: training is opt-out, not excluded by default, per its DPA"
        );
        assert_eq!(mistral.risk_rating, 2);
        assert_eq!(
            mistral.privacy_guardian_default_level,
            Some(PrivacyGuardianDefaultLevel::Medium)
        );
    }

    /// items.id=430/432: list_providers_by_type is the flag-based
    /// replacement for a hardcoded ["mistral", "groq"] array -- must return
    /// exactly the seeded Tier 1.5 set and nothing else (Duck.ai/Claude/
    /// ChatGPT/Gemini are a different provider_type).
    #[tokio::test]
    async fn list_providers_by_type_returns_only_matching_active_providers() {
        let _lock = crate::test_support::ENV_MUTEX.lock().await;
        let saved_root = std::env::var("QR_DATA_ROOT").ok();
        let tempdir = tempfile::tempdir().expect("failed to create tempdir");
        std::env::set_var("QR_DATA_ROOT", tempdir.path());

        let result = crate::persistence::migrations::migrate_shared_db().await;

        let outcome = async {
            let pool = sqlx::SqlitePool::connect_with(
                crate::providers::utils::connect_options_unencrypted(
                    &crate::providers::utils::db_path_shared(),
                ),
            )
            .await?;
            let cloud_api = list_providers_by_type(&pool, "cloud_inference_api").await?;
            let ids: Vec<&str> = cloud_api.iter().map(|p| p.id.as_str()).collect();
            assert_eq!(
                ids.len(),
                2,
                "only groq and mistral are cloud_inference_api"
            );
            assert!(ids.contains(&"groq"));
            assert!(ids.contains(&"mistral"));

            let external = list_providers_by_type(&pool, "external_service").await?;
            let external_ids: Vec<&str> = external.iter().map(|p| p.id.as_str()).collect();
            assert!(!external_ids.contains(&"groq"));
            assert!(!external_ids.contains(&"mistral"));

            Ok::<(), ProviderStoreError>(())
        }
        .await;

        if let Some(v) = saved_root {
            std::env::set_var("QR_DATA_ROOT", v);
        } else {
            std::env::remove_var("QR_DATA_ROOT");
        }

        result.expect("migrate_shared_db must succeed");
        outcome.expect("list_providers_by_type assertions must pass");
    }
}
