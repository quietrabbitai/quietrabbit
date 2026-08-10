// src-tauri/src/persistence/provider_store.rs
//
// tier3_providers CRUD for shared.db (unencrypted) -- items.id=202.
// Schema: shared_001.sql's tier3_providers table (added 2026-08-04), per
// decisions.id=684 (schema required, scoped with items.id=186's curation
// policy) and decisions.id=710 (items.id=186 scoped -- policy this table
// expresses). See shared_001.sql's own tier3_providers header for full
// column-by-column rationale; not re-derived here.
//
// Backs TIER3_ACCESS_MODEL.md's selector screen (State 3, decisions.id=681)
// -- list_active_providers() is that screen's primary read path, split by
// tier for the two-box layout.
//
// QUERY STYLE: runtime sqlx::query() only -- no query!() macros, matching
// every other store in this module (persona_store.rs, focus_settings_store.rs).
// shared.db is unencrypted -- no PRAGMA key required.
//
// CONNECTION MODEL: one connection per call, matching persona_store.rs
// (Phase 1 correctness implementation, not yet pooled).
//
// WRITE ACCESS: this module provides full CRUD (create/update/deactivate),
// but per decisions.id=710(b) the curated list is release-bundled, not
// user-editable at runtime -- no IPC command surface exposes these writes
// to the frontend. Writes are for release-time seeding (a future seed
// script/migration) and any future Chat-PM-directed catalog maintenance,
// not end-user action. Flagged here so a future reader doesn't assume a
// missing write-path IPC command is an oversight.
//
// SEED DATA: NOT included in this module. Populating real, verified rows
// for Duck.ai/Brave Leo/Claude/ChatGPT/Gemini against decisions.id=710(a)'s
// documentation-gate criteria is research/content work, out of scope here
// -- see shared_001.sql's tier3_providers header and this session's handoff.

use sqlx::sqlite::SqliteConnectOptions;
use sqlx::ConnectOptions;
use sqlx::Row;
use sqlx::SqliteConnection;
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

/// tier: 2 or 3, mirrors TIER3_ACCESS_MODEL.md's two lanes exactly --
/// enforced by the DB's own CHECK, re-validated here at the Rust boundary
/// so a bad value is rejected before it reaches sqlx, not just at INSERT.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderTier {
    Tier2 = 2,
    Tier3 = 3,
}

impl ProviderTier {
    fn as_i64(self) -> i64 {
        self as i64
    }

    fn from_i64(v: i64) -> Result<Self, ProviderStoreError> {
        match v {
            2 => Ok(ProviderTier::Tier2),
            3 => Ok(ProviderTier::Tier3),
            other => Err(ProviderStoreError::Validation(format!(
                "tier must be 2 or 3, got {other} -- schema CHECK should have \
                 rejected this at write time; seeing it here means a row was \
                 written outside this module or the CHECK itself drifted."
            ))),
        }
    }
}

/// mode: only 'embedded_web' is used for R1. 'api' is reserved per
/// shared_001.sql's tier3_providers header -- see that header before
/// adding Api-specific fields to this struct; none exist yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderMode {
    EmbeddedWeb,
    Api,
}

impl ProviderMode {
    fn as_str(self) -> &'static str {
        match self {
            ProviderMode::EmbeddedWeb => "embedded_web",
            ProviderMode::Api => "api",
        }
    }

    fn from_str(s: &str) -> Result<Self, ProviderStoreError> {
        match s {
            "embedded_web" => Ok(ProviderMode::EmbeddedWeb),
            "api" => Ok(ProviderMode::Api),
            other => Err(ProviderStoreError::Validation(format!(
                "mode must be 'embedded_web' or 'api', got '{other}' -- schema \
                 CHECK should have rejected this at write time."
            ))),
        }
    }
}

/// activation_status: 'active' | 'deprecated' only -- deliberately no
/// richer state machine, per shared_001.sql's tier3_providers header
/// (decisions.id=710(b): release-bundled, not runtime-activated).
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

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Provider {
    pub id: String,
    pub display_name: String,
    pub tier: ProviderTier,
    pub mode: ProviderMode,
    pub launch_url: Option<String>,
    pub login_required: bool,
    pub activation_status: ActivationStatus,
    /// Stored as JSON TEXT in DB. Default is empty object {}. Holds
    /// decisions.id=710(a)'s documentation-gate fields (ToS/retention
    /// citation, jurisdiction, contradictory-report notes) -- also this
    /// provider's selector-card retention-posture display source, per
    /// shared_001.sql's CARD DISPLAY note.
    pub documentation_gate: serde_json::Value,
    pub last_reviewed_at: Option<String>,
    pub review_trigger_note: Option<String>,
    pub created_at: String,
}

// ---------------------------------------------------------------------------
// DB opener (shared.db — unencrypted)
// ---------------------------------------------------------------------------

async fn open_shared_db() -> Result<SqliteConnection, ProviderStoreError> {
    let db_path = crate::persistence::migrations::get_data_root()
        .join("instance")
        .join("shared.db");
    let network_storage = std::env::var("QR_NETWORK_STORAGE")
        .map(|v| v.to_lowercase() == "true")
        .unwrap_or(false);
    let journal_mode = if network_storage { "DELETE" } else { "WAL" };

    let conn = SqliteConnectOptions::new()
        .filename(&db_path)
        .create_if_missing(false)
        .pragma("journal_mode", journal_mode)
        .connect()
        .await?;

    Ok(conn)
}

// ---------------------------------------------------------------------------
// Row extraction
// ---------------------------------------------------------------------------

fn row_to_provider(row: &sqlx::sqlite::SqliteRow) -> Result<Provider, ProviderStoreError> {
    let id: String = row.try_get("id").map_err(ProviderStoreError::Database)?;
    let tier_raw: i64 = row.try_get("tier").map_err(ProviderStoreError::Database)?;
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

    let documentation_gate: serde_json::Value =
        serde_json::from_str(&doc_gate_raw).unwrap_or_else(|e| {
            log::warn!(
                "provider '{id}' documentation_gate failed to parse as JSON, \
                 defaulting to empty object: {e}"
            );
            serde_json::Value::Object(serde_json::Map::new())
        });

    Ok(Provider {
        id,
        display_name: row
            .try_get("display_name")
            .map_err(ProviderStoreError::Database)?,
        tier: ProviderTier::from_i64(tier_raw)?,
        mode: ProviderMode::from_str(&mode_raw)?,
        launch_url: row
            .try_get("launch_url")
            .map_err(ProviderStoreError::Database)?,
        login_required: login_required_raw != 0,
        activation_status: ActivationStatus::from_str(&activation_status_raw)?,
        documentation_gate,
        last_reviewed_at: row
            .try_get("last_reviewed_at")
            .map_err(ProviderStoreError::Database)?,
        review_trigger_note: row
            .try_get("review_trigger_note")
            .map_err(ProviderStoreError::Database)?,
        created_at: row
            .try_get("created_at")
            .map_err(ProviderStoreError::Database)?,
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
pub async fn get_provider(provider_id: &str) -> Result<Option<Provider>, ProviderStoreError> {
    let mut conn = open_shared_db().await?;

    let row = sqlx::query(
        "SELECT id, display_name, tier, mode, launch_url, login_required,
                activation_status, documentation_gate, last_reviewed_at,
                review_trigger_note, created_at
         FROM tier3_providers WHERE id = ?",
    )
    .bind(provider_id)
    .fetch_optional(&mut conn)
    .await?;

    match row {
        None => Ok(None),
        Some(r) => Ok(Some(row_to_provider(&r)?)),
    }
}

/// The selector screen's primary read path (TIER3_ACCESS_MODEL.md State 3):
/// all 'active' providers, ordered tier then display_name so a caller can
/// split the result into the two selector boxes without a second query --
/// tier ASC puts Tier 2 ("no login required") rows first, matching the
/// spec's top-box/bottom-box ordering.
pub async fn list_active_providers() -> Result<Vec<Provider>, ProviderStoreError> {
    let mut conn = open_shared_db().await?;

    let rows = sqlx::query(
        "SELECT id, display_name, tier, mode, launch_url, login_required,
                activation_status, documentation_gate, last_reviewed_at,
                review_trigger_note, created_at
         FROM tier3_providers
         WHERE activation_status = 'active'
         ORDER BY tier ASC, display_name ASC",
    )
    .fetch_all(&mut conn)
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
pub async fn list_all_providers() -> Result<Vec<Provider>, ProviderStoreError> {
    let mut conn = open_shared_db().await?;

    let rows = sqlx::query(
        "SELECT id, display_name, tier, mode, launch_url, login_required,
                activation_status, documentation_gate, last_reviewed_at,
                review_trigger_note, created_at
         FROM tier3_providers
         ORDER BY tier ASC, display_name ASC",
    )
    .fetch_all(&mut conn)
    .await?;

    let mut providers = Vec::new();
    for r in rows {
        providers.push(row_to_provider(&r)?);
    }
    Ok(providers)
}

// ---------------------------------------------------------------------------
// Write operations
// ---------------------------------------------------------------------------

/// Create a new provider catalog row. Release-time/catalog-maintenance use
/// only (see module header WRITE ACCESS note) -- not exposed via IPC.
/// Returns Err(AlreadyExists) if provider_id already exists.
#[allow(clippy::too_many_arguments)] // Explicit boundary matching the row shape; see focus_settings_store.rs's record_friction_gate_decision precedent.
pub async fn create_provider(
    provider_id: &str,
    display_name: &str,
    tier: ProviderTier,
    mode: ProviderMode,
    launch_url: Option<&str>,
    login_required: bool,
    documentation_gate: &serde_json::Value,
) -> Result<Provider, ProviderStoreError> {
    if mode == ProviderMode::EmbeddedWeb && launch_url.is_none() {
        return Err(ProviderStoreError::Validation(
            "launch_url is required when mode='embedded_web' -- the pane has \
             nothing to point CEF at otherwise."
                .to_owned(),
        ));
    }

    let created_at = crate::providers::utils::now();
    let doc_gate_str = serde_json::to_string(documentation_gate).map_err(|e| {
        ProviderStoreError::Validation(format!("documentation_gate not valid JSON: {e}"))
    })?;
    let mut conn = open_shared_db().await?;

    sqlx::query(
        "INSERT INTO tier3_providers
         (id, display_name, tier, mode, launch_url, login_required,
          activation_status, documentation_gate, last_reviewed_at,
          review_trigger_note, created_at)
         VALUES (?, ?, ?, ?, ?, ?, 'active', ?, NULL, NULL, ?)",
    )
    .bind(provider_id)
    .bind(display_name)
    .bind(tier.as_i64())
    .bind(mode.as_str())
    .bind(launch_url)
    .bind(login_required as i64)
    .bind(&doc_gate_str)
    .bind(&created_at)
    .execute(&mut conn)
    .await
    .map_err(|e| classify_constraint_error(provider_id, e))?;

    Ok(Provider {
        id: provider_id.to_owned(),
        display_name: display_name.to_owned(),
        tier,
        mode,
        launch_url: launch_url.map(|s| s.to_owned()),
        login_required,
        activation_status: ActivationStatus::Active,
        documentation_gate: documentation_gate.clone(),
        last_reviewed_at: None,
        review_trigger_note: None,
        created_at,
    })
}

/// Set activation_status. The only status transition this table's own
/// lifecycle needs (decisions.id=710(b): release-bundled, no richer state
/// machine) -- 'deprecated' rows stay in the table for audit/history
/// rather than being deleted.
pub async fn set_activation_status(
    provider_id: &str,
    status: ActivationStatus,
) -> Result<(), ProviderStoreError> {
    let mut conn = open_shared_db().await?;

    let result = sqlx::query("UPDATE tier3_providers SET activation_status = ? WHERE id = ?")
        .bind(status.as_str())
        .bind(provider_id)
        .execute(&mut conn)
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
    let mut conn = open_shared_db().await?;

    let result = sqlx::query(
        "UPDATE tier3_providers
         SET last_reviewed_at = ?, review_trigger_note = ?
         WHERE id = ?",
    )
    .bind(&reviewed_at)
    .bind(trigger_note)
    .bind(provider_id)
    .execute(&mut conn)
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
    provider_id: &str,
    documentation_gate: &serde_json::Value,
) -> Result<(), ProviderStoreError> {
    let doc_gate_str = serde_json::to_string(documentation_gate).map_err(|e| {
        ProviderStoreError::Validation(format!("documentation_gate not valid JSON: {e}"))
    })?;
    let mut conn = open_shared_db().await?;

    let result = sqlx::query("UPDATE tier3_providers SET documentation_gate = ? WHERE id = ?")
        .bind(&doc_gate_str)
        .bind(provider_id)
        .execute(&mut conn)
        .await?;

    if result.rows_affected() == 0 {
        return Err(ProviderStoreError::NotFound(provider_id.to_owned()));
    }
    Ok(())
}
