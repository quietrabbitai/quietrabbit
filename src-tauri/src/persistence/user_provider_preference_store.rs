// src-tauri/src/persistence/user_provider_preference_store.rs
//
// user_provider_preference CRUD for shared.db (unencrypted) -- items.id=428
// (PROVIDER_REGISTRY_AND_TIER_MODEL_SPEC.md Part 2b). Per-user configuration
// scoped User x Persona x Focus, cascading most-specific-match-wins
// (Focus > Persona > account-wide). See shared_013.sql's own
// user_provider_preference header for full column-by-column rationale; not
// re-derived here.
//
// PRECEDENCE IMPLEMENTATION NOTE: the spec describes this as "the same
// pattern user_capabilities already does precedence" -- verified this
// session that no such Rust implementation actually exists anywhere in
// this codebase. shared_001.sql's user_capabilities comment describes
// Focus>Persona>account-wide precedence in prose but defers the real
// algorithm to AUTH_MULTIUSER_ARCHITECTURE.md (not present in this repo),
// and R1 only ever writes account-wide user_capabilities rows -- there is
// no existing Rust function to port. resolve_preference() below is written
// fresh from the spec's own text (most-specific-match-wins: Focus row,
// then Persona row, then account-wide row, then None), not a mirror of
// prior code.
//
// SCOPE: this module provides the table's CRUD and the precedence read
// (items.id=428's stated scope). No IPC command surface is exposed --
// writing real rows here is onboarding-flow work (Part 4) and Focus
// Builder UI work (Part 3c), both explicitly out of scope this session,
// matching provider_store.rs's own precedent of full CRUD with no IPC
// surface until a real caller needs one.
//
// QUERY STYLE / CONNECTION MODEL: matches provider_store.rs exactly --
// runtime sqlx::query() only, one connection per call, shared.db
// (unencrypted, no PRAGMA key required).

use sqlx::Row;
use thiserror::Error;

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum UserProviderPreferenceStoreError {
    #[error("Database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("Preference row '{0}' not found")]
    NotFound(String),
    #[error("Validation error: {0}")]
    Validation(String),
}

// ---------------------------------------------------------------------------
// Data types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, specta::Type)]
#[serde(rename_all = "lowercase")]
pub enum UserPreference {
    Preferred,
    Allowed,
    Declined,
}

impl UserPreference {
    fn as_str(self) -> &'static str {
        match self {
            UserPreference::Preferred => "preferred",
            UserPreference::Allowed => "allowed",
            UserPreference::Declined => "declined",
        }
    }

    fn from_str(s: &str) -> Result<Self, UserProviderPreferenceStoreError> {
        match s {
            "preferred" => Ok(UserPreference::Preferred),
            "allowed" => Ok(UserPreference::Allowed),
            "declined" => Ok(UserPreference::Declined),
            other => Err(UserProviderPreferenceStoreError::Validation(format!(
                "user_preference must be 'preferred', 'allowed', or 'declined', got '{other}' \
                 -- schema CHECK should have rejected this at write time."
            ))),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, specta::Type)]
#[serde(rename_all = "lowercase")]
pub enum SubscriptionStatus {
    Free,
    Paid,
}

impl SubscriptionStatus {
    fn as_str(self) -> &'static str {
        match self {
            SubscriptionStatus::Free => "free",
            SubscriptionStatus::Paid => "paid",
        }
    }

    fn from_str(s: &str) -> Result<Self, UserProviderPreferenceStoreError> {
        match s {
            "free" => Ok(SubscriptionStatus::Free),
            "paid" => Ok(SubscriptionStatus::Paid),
            other => Err(UserProviderPreferenceStoreError::Validation(format!(
                "subscription_status must be 'free' or 'paid', got '{other}' -- schema CHECK \
                 should have rejected this at write time."
            ))),
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, specta::Type)]
pub struct UserProviderPreference {
    pub id: String,
    pub user_id: String,
    /// NULL = account-wide default (Part 2b's NULL-means-account-wide
    /// convention, mirroring user_capabilities).
    pub persona_id: Option<String>,
    /// NULL = Persona-wide default; set = Focus-specific override.
    pub focus_id: Option<String>,
    pub provider_id: String,
    pub login_available: bool,
    pub user_preference: UserPreference,
    pub enabled_at: Option<String>,
    pub declined_at: Option<String>,
    pub local_model_version: Option<String>,
    pub installed_at: Option<String>,
    pub last_verified_at: Option<String>,
    pub subscription_status: Option<SubscriptionStatus>,
    pub created_at: String,
    /// items.id=708 (shared_026.sql, decisions.id=875): the user's plan at
    /// this provider, matched against providers.plan_inclusion.
    pub plan_key: Option<String>,
    /// items.id=708: the selected model, resolved Focus, then Persona, then
    /// account like every other column. No FK: an uncurated model from a
    /// live list is allowed.
    pub model_id: Option<String>,
}

/// Input to upsert_preference(). A plain struct rather than a long
/// positional argument list, matching provider_store::NewProvider's own
/// precedent for a row shape this wide.
pub struct NewUserProviderPreference<'a> {
    pub user_id: &'a str,
    pub persona_id: Option<&'a str>,
    pub focus_id: Option<&'a str>,
    pub provider_id: &'a str,
    pub login_available: bool,
    pub user_preference: UserPreference,
    pub local_model_version: Option<&'a str>,
    pub subscription_status: Option<SubscriptionStatus>,
    /// items.id=708. upsert_preference overwrites these two on an existing
    /// row exactly as it overwrites subscription_status: a caller that does
    /// not mean to change the selection must pass the current values.
    pub plan_key: Option<&'a str>,
    pub model_id: Option<&'a str>,
}

// ---------------------------------------------------------------------------
// DB opener (shared.db — unencrypted)
// ---------------------------------------------------------------------------

const SELECT_COLUMNS: &str = "id, user_id, persona_id, focus_id, provider_id,
                login_available, user_preference, enabled_at, declined_at,
                local_model_version, installed_at, last_verified_at,
                subscription_status, created_at, plan_key, model_id";

// ---------------------------------------------------------------------------
// Row extraction
// ---------------------------------------------------------------------------

fn row_to_preference(
    row: &sqlx::sqlite::SqliteRow,
) -> Result<UserProviderPreference, UserProviderPreferenceStoreError> {
    let login_available_raw: i64 = row
        .try_get("login_available")
        .map_err(UserProviderPreferenceStoreError::Database)?;
    let user_preference_raw: String = row
        .try_get("user_preference")
        .map_err(UserProviderPreferenceStoreError::Database)?;
    let subscription_status_raw: Option<String> = row
        .try_get("subscription_status")
        .map_err(UserProviderPreferenceStoreError::Database)?;

    let subscription_status = subscription_status_raw
        .as_deref()
        .map(SubscriptionStatus::from_str)
        .transpose()?;

    Ok(UserProviderPreference {
        id: row
            .try_get("id")
            .map_err(UserProviderPreferenceStoreError::Database)?,
        user_id: row
            .try_get("user_id")
            .map_err(UserProviderPreferenceStoreError::Database)?,
        persona_id: row
            .try_get("persona_id")
            .map_err(UserProviderPreferenceStoreError::Database)?,
        focus_id: row
            .try_get("focus_id")
            .map_err(UserProviderPreferenceStoreError::Database)?,
        provider_id: row
            .try_get("provider_id")
            .map_err(UserProviderPreferenceStoreError::Database)?,
        login_available: login_available_raw != 0,
        user_preference: UserPreference::from_str(&user_preference_raw)?,
        enabled_at: row
            .try_get("enabled_at")
            .map_err(UserProviderPreferenceStoreError::Database)?,
        declined_at: row
            .try_get("declined_at")
            .map_err(UserProviderPreferenceStoreError::Database)?,
        local_model_version: row
            .try_get("local_model_version")
            .map_err(UserProviderPreferenceStoreError::Database)?,
        installed_at: row
            .try_get("installed_at")
            .map_err(UserProviderPreferenceStoreError::Database)?,
        last_verified_at: row
            .try_get("last_verified_at")
            .map_err(UserProviderPreferenceStoreError::Database)?,
        subscription_status,
        created_at: row
            .try_get("created_at")
            .map_err(UserProviderPreferenceStoreError::Database)?,
        plan_key: row
            .try_get("plan_key")
            .map_err(UserProviderPreferenceStoreError::Database)?,
        model_id: row
            .try_get("model_id")
            .map_err(UserProviderPreferenceStoreError::Database)?,
    })
}

// ---------------------------------------------------------------------------
// Read operations
// ---------------------------------------------------------------------------

/// items.id=428: the most-specific-match-wins read (Part 2b) -- tries the
/// Focus-scoped row first (only if focus_id given), then the Persona-scoped
/// row (only if persona_id given), then the account-wide row. Returns None
/// if no row exists at any scope -- per Part 2b's explicit new-user
/// convention, callers must apply a system-wide hardcoded default in that
/// case, never inherit from another user's rows.
///
/// Three sequential queries rather than one UNION/priority-ordered query:
/// simplest direct reading of "most-specific-match-wins," and matches this
/// codebase's own preference for small explicit queries over CTEs
/// (persona_store.rs style) at this data volume (at most 3 rows could ever
/// match for a given user+provider).
pub async fn resolve_preference(
    pool: &sqlx::SqlitePool,
    user_id: &str,
    persona_id: Option<&str>,
    focus_id: Option<&str>,
    provider_id: &str,
) -> Result<Option<UserProviderPreference>, UserProviderPreferenceStoreError> {
    let mut conn = pool.acquire().await?;

    if let Some(focus_id) = focus_id {
        let sql = format!(
            "SELECT {SELECT_COLUMNS} FROM user_provider_preference
             WHERE user_id = ? AND focus_id = ? AND provider_id = ?"
        );
        if let Some(row) = sqlx::query(&sql)
            .bind(user_id)
            .bind(focus_id)
            .bind(provider_id)
            .fetch_optional(&mut *conn)
            .await?
        {
            return Ok(Some(row_to_preference(&row)?));
        }
    }

    if let Some(persona_id) = persona_id {
        let sql = format!(
            "SELECT {SELECT_COLUMNS} FROM user_provider_preference
             WHERE user_id = ? AND persona_id = ? AND focus_id IS NULL AND provider_id = ?"
        );
        if let Some(row) = sqlx::query(&sql)
            .bind(user_id)
            .bind(persona_id)
            .bind(provider_id)
            .fetch_optional(&mut *conn)
            .await?
        {
            return Ok(Some(row_to_preference(&row)?));
        }
    }

    let sql = format!(
        "SELECT {SELECT_COLUMNS} FROM user_provider_preference
         WHERE user_id = ? AND persona_id IS NULL AND focus_id IS NULL AND provider_id = ?"
    );
    let row = sqlx::query(&sql)
        .bind(user_id)
        .bind(provider_id)
        .fetch_optional(&mut *conn)
        .await?;

    match row {
        None => Ok(None),
        Some(r) => Ok(Some(row_to_preference(&r)?)),
    }
}

/// items.id=432: picks the Preferred provider across several candidates
/// (e.g. the qr_hosted set -- resolve_preference() itself only answers "what
/// is the preference for this ONE provider_id," but lifecycle.rs needs to
/// choose BETWEEN hosted providers, not resolve one named provider).
/// Resolves each candidate independently at the given scope and returns the
/// single one whose resolved preference is Preferred. Zero matches -> None,
/// matching this codebase's existing "no prescribed default" behavior
/// (surfaces MissingQrHostedConfig downstream, same as today). More than one
/// match is a data anomaly the schema doesn't prevent (nothing stops two
/// different providers both being marked Preferred at the same scope) --
/// also returns None rather than an arbitrary pick, consistent with this
/// codebase's existing unreachable!()-guarded "never silently guess"
/// pattern in executor.rs.
pub async fn find_preferred_provider(
    pool: &sqlx::SqlitePool,
    user_id: &str,
    persona_id: Option<&str>,
    focus_id: Option<&str>,
    candidate_provider_ids: &[String],
) -> Result<Option<String>, UserProviderPreferenceStoreError> {
    let mut preferred =
        preferred_providers(pool, user_id, persona_id, focus_id, candidate_provider_ids).await?;
    match preferred.len() {
        1 => Ok(preferred.pop()),
        _ => Ok(None),
    }
}

/// Every candidate whose resolved preference at the given scope is
/// Preferred, in candidate order. find_preferred_provider() is the
/// "exactly one" view of this; conductor::provider_selection also needs the
/// full set so it can tell "nothing preferred" apart from "the preferred
/// provider is excluded by the Focus's external-access ceiling".
pub async fn preferred_providers(
    pool: &sqlx::SqlitePool,
    user_id: &str,
    persona_id: Option<&str>,
    focus_id: Option<&str>,
    candidate_provider_ids: &[String],
) -> Result<Vec<String>, UserProviderPreferenceStoreError> {
    let mut preferred: Vec<String> = Vec::new();
    for provider_id in candidate_provider_ids {
        if let Some(pref) =
            resolve_preference(pool, user_id, persona_id, focus_id, provider_id).await?
        {
            if pref.user_preference == UserPreference::Preferred {
                preferred.push(provider_id.clone());
            }
        }
    }
    Ok(preferred)
}

/// Raw lookup at an exact scope (no cascading) -- for callers that need to
/// know whether a specific-scope override row exists, distinct from
/// resolve_preference()'s cascading result.
pub async fn get_preference_at_scope(
    pool: &sqlx::SqlitePool,
    user_id: &str,
    persona_id: Option<&str>,
    focus_id: Option<&str>,
    provider_id: &str,
) -> Result<Option<UserProviderPreference>, UserProviderPreferenceStoreError> {
    let mut conn = pool.acquire().await?;

    let sql = format!(
        "SELECT {SELECT_COLUMNS} FROM user_provider_preference
         WHERE user_id = ?
           AND ((? IS NULL AND persona_id IS NULL) OR persona_id = ?)
           AND ((? IS NULL AND focus_id IS NULL) OR focus_id = ?)
           AND provider_id = ?"
    );
    let row = sqlx::query(&sql)
        .bind(user_id)
        .bind(persona_id)
        .bind(persona_id)
        .bind(focus_id)
        .bind(focus_id)
        .bind(provider_id)
        .fetch_optional(&mut *conn)
        .await?;

    match row {
        None => Ok(None),
        Some(r) => Ok(Some(row_to_preference(&r)?)),
    }
}

/// All preference rows for a user, across every scope -- for a future
/// settings-surface listing, not a hot read path.
pub async fn list_preferences_for_user(
    pool: &sqlx::SqlitePool,
    user_id: &str,
) -> Result<Vec<UserProviderPreference>, UserProviderPreferenceStoreError> {
    let mut conn = pool.acquire().await?;

    let sql = format!(
        "SELECT {SELECT_COLUMNS} FROM user_provider_preference
         WHERE user_id = ?
         ORDER BY provider_id ASC, persona_id ASC, focus_id ASC"
    );
    let rows = sqlx::query(&sql)
        .bind(user_id)
        .fetch_all(&mut *conn)
        .await?;

    let mut prefs = Vec::new();
    for r in rows {
        prefs.push(row_to_preference(&r)?);
    }
    Ok(prefs)
}

// ---------------------------------------------------------------------------
// Write operations
// ---------------------------------------------------------------------------

/// Insert or replace the preference row at the given scope (user_id x
/// persona_id x focus_id x provider_id) -- explicit check-then-write, not
/// `INSERT ... ON CONFLICT`, since the partial unique indexes are keyed
/// per scope level and SQLite can't target a partial index with a single
/// ON CONFLICT clause that covers all three. Mirrors
/// get_preference_at_scope()'s own scope-matching WHERE shape so "does a
/// row exist here" and "write to that exact row" never drift apart.
pub async fn upsert_preference(
    pool: &sqlx::SqlitePool,
    new: NewUserProviderPreference<'_>,
) -> Result<UserProviderPreference, UserProviderPreferenceStoreError> {
    let existing = get_preference_at_scope(
        pool,
        new.user_id,
        new.persona_id,
        new.focus_id,
        new.provider_id,
    )
    .await?;

    let now = crate::providers::utils::now();
    let mut conn = pool.acquire().await?;

    if let Some(existing) = existing {
        sqlx::query(
            "UPDATE user_provider_preference
             SET login_available = ?, user_preference = ?, local_model_version = ?,
                 subscription_status = ?, plan_key = ?, model_id = ?,
                 enabled_at = CASE WHEN ? = 'declined' THEN enabled_at ELSE ? END,
                 declined_at = CASE WHEN ? = 'declined' THEN ? ELSE declined_at END
             WHERE id = ?",
        )
        .bind(new.login_available as i64)
        .bind(new.user_preference.as_str())
        .bind(new.local_model_version)
        .bind(new.subscription_status.map(|s| s.as_str()))
        .bind(new.plan_key)
        .bind(new.model_id)
        .bind(new.user_preference.as_str())
        .bind(&now)
        .bind(new.user_preference.as_str())
        .bind(&now)
        .bind(&existing.id)
        .execute(&mut *conn)
        .await?;

        return get_preference_at_scope(
            pool,
            new.user_id,
            new.persona_id,
            new.focus_id,
            new.provider_id,
        )
        .await?
        .ok_or_else(|| UserProviderPreferenceStoreError::NotFound(existing.id.clone()));
    }

    let id = uuid::Uuid::new_v4().to_string();
    let (enabled_at, declined_at) = if new.user_preference == UserPreference::Declined {
        (None, Some(now.clone()))
    } else {
        (Some(now.clone()), None)
    };

    sqlx::query(
        "INSERT INTO user_provider_preference
         (id, user_id, persona_id, focus_id, provider_id, login_available,
          user_preference, enabled_at, declined_at, local_model_version,
          installed_at, last_verified_at, subscription_status, created_at,
          plan_key, model_id)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NULL, NULL, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(new.user_id)
    .bind(new.persona_id)
    .bind(new.focus_id)
    .bind(new.provider_id)
    .bind(new.login_available as i64)
    .bind(new.user_preference.as_str())
    .bind(&enabled_at)
    .bind(&declined_at)
    .bind(new.local_model_version)
    .bind(new.subscription_status.map(|s| s.as_str()))
    .bind(&now)
    .bind(new.plan_key)
    .bind(new.model_id)
    .execute(&mut *conn)
    .await?;

    Ok(UserProviderPreference {
        id,
        user_id: new.user_id.to_owned(),
        persona_id: new.persona_id.map(|s| s.to_owned()),
        focus_id: new.focus_id.map(|s| s.to_owned()),
        provider_id: new.provider_id.to_owned(),
        login_available: new.login_available,
        user_preference: new.user_preference,
        enabled_at,
        declined_at,
        local_model_version: new.local_model_version.map(|s| s.to_owned()),
        installed_at: None,
        last_verified_at: None,
        subscription_status: new.subscription_status,
        created_at: now,
        plan_key: new.plan_key.map(|s| s.to_owned()),
        model_id: new.model_id.map(|s| s.to_owned()),
    })
}

// ---------------------------------------------------------------------------
// Model selection (items.id=711, decisions.id=875 point 5)
// ---------------------------------------------------------------------------
//
// The selected model lives in `model_id` (shared_026.sql) on the same rows as
// the provider preference. It resolves PER COLUMN: the first non-NULL
// `model_id` among the Focus row, the Persona row and the account row, for the
// provider already chosen. That is deliberately not resolve_preference()'s
// whole-row rule: a narrower row created only to hold a model must not hide
// the account row's model.
//
// A narrower row IS a preference override (resolve_preference() returns the
// narrowest row whole), so set_model_selection() never invents a preference:
// a new narrower row copies `user_preference` and `login_available` from what
// the scope resolves to now, or is `allowed` when the provider has no row at
// all (never Preferred, so it cannot make a provider the choice by itself).
// KNOWN LIMITATION, pinned by a test in conductor::provider_selection: the
// copy goes stale if the account-level provider is later changed. Two
// Preferred providers then resolve at that scope and the provider resolver
// returns NotConfigured. That fails closed (the step stops and asks); it
// never routes to a provider the user did not choose.

/// Where a model selection is stored.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, specta::Type)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SelectionScope {
    Account,
    Persona { persona_id: String },
    Focus { focus_id: String },
}

/// Which level a resolved selection came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, specta::Type)]
#[serde(rename_all = "snake_case")]
pub enum SelectionSource {
    Focus,
    Persona,
    Account,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedModelSelection {
    pub model_id: String,
    pub source: SelectionSource,
}

/// The selected model for a provider at this Persona and Focus, per column
/// (see the section comment). `None` when no level selects one; the caller
/// then uses the provider's default model.
pub async fn resolve_model_selection(
    pool: &sqlx::SqlitePool,
    user_id: &str,
    persona_id: Option<&str>,
    focus_id: Option<&str>,
    provider_id: &str,
) -> Result<Option<ResolvedModelSelection>, UserProviderPreferenceStoreError> {
    let mut conn = pool.acquire().await?;

    if let Some(focus_id) = focus_id.filter(|s| !s.is_empty()) {
        let model: Option<Option<String>> = sqlx::query_scalar(
            "SELECT model_id FROM user_provider_preference
             WHERE user_id = ? AND focus_id = ? AND provider_id = ?",
        )
        .bind(user_id)
        .bind(focus_id)
        .bind(provider_id)
        .fetch_optional(&mut *conn)
        .await?;
        if let Some(Some(model_id)) = model {
            return Ok(Some(ResolvedModelSelection {
                model_id,
                source: SelectionSource::Focus,
            }));
        }
    }

    if let Some(persona_id) = persona_id.filter(|s| !s.is_empty()) {
        let model: Option<Option<String>> = sqlx::query_scalar(
            "SELECT model_id FROM user_provider_preference
             WHERE user_id = ? AND persona_id = ? AND focus_id IS NULL AND provider_id = ?",
        )
        .bind(user_id)
        .bind(persona_id)
        .bind(provider_id)
        .fetch_optional(&mut *conn)
        .await?;
        if let Some(Some(model_id)) = model {
            return Ok(Some(ResolvedModelSelection {
                model_id,
                source: SelectionSource::Persona,
            }));
        }
    }

    let model: Option<Option<String>> = sqlx::query_scalar(
        "SELECT model_id FROM user_provider_preference
         WHERE user_id = ? AND persona_id IS NULL AND focus_id IS NULL AND provider_id = ?",
    )
    .bind(user_id)
    .bind(provider_id)
    .fetch_optional(&mut *conn)
    .await?;
    Ok(model.flatten().map(|model_id| ResolvedModelSelection {
        model_id,
        source: SelectionSource::Account,
    }))
}

/// Set (or, with `None`, clear) the model selected for `provider_id` at one
/// scope. Writes ONLY `model_id` on an existing row at exactly that scope.
/// With no row there, a selection creates one that copies the preference the
/// scope resolves to (see the section comment); clearing with no row is a
/// no-op. `persona_context` is the Persona a Focus belongs to, used only to
/// find the preference to copy for a new Focus row.
pub async fn set_model_selection(
    pool: &sqlx::SqlitePool,
    user_id: &str,
    scope: &SelectionScope,
    persona_context: Option<&str>,
    provider_id: &str,
    model_id: Option<&str>,
) -> Result<(), UserProviderPreferenceStoreError> {
    let (persona_id, focus_id): (Option<&str>, Option<&str>) = match scope {
        SelectionScope::Account => (None, None),
        SelectionScope::Persona { persona_id } => (Some(persona_id.as_str()), None),
        SelectionScope::Focus { focus_id } => (None, Some(focus_id.as_str())),
    };
    if matches!(scope, SelectionScope::Persona { persona_id } if persona_id.is_empty())
        || matches!(scope, SelectionScope::Focus { focus_id } if focus_id.is_empty())
    {
        return Err(UserProviderPreferenceStoreError::Validation(
            "a model selection scope needs a non-empty id".to_owned(),
        ));
    }

    let existing =
        get_preference_at_scope(pool, user_id, persona_id, focus_id, provider_id).await?;

    if let Some(existing) = existing {
        let mut conn = pool.acquire().await?;
        sqlx::query("UPDATE user_provider_preference SET model_id = ? WHERE id = ?")
            .bind(model_id)
            .bind(&existing.id)
            .execute(&mut *conn)
            .await?;
        return Ok(());
    }
    let Some(model_id) = model_id else {
        return Ok(());
    };

    // Nothing at this scope yet: copy what it resolves to today.
    let resolve_persona = persona_id.or(persona_context);
    let inherited =
        resolve_preference(pool, user_id, resolve_persona, focus_id, provider_id).await?;
    let (login_available, user_preference, subscription_status, plan_key) = match &inherited {
        Some(p) => (
            p.login_available,
            p.user_preference,
            p.subscription_status,
            p.plan_key.clone(),
        ),
        None => (false, UserPreference::Allowed, None, None),
    };
    upsert_preference(
        pool,
        NewUserProviderPreference {
            user_id,
            persona_id,
            focus_id,
            provider_id,
            login_available,
            user_preference,
            local_model_version: None,
            subscription_status,
            plan_key: plan_key.as_deref(),
            model_id: Some(model_id),
        },
    )
    .await?;
    Ok(())
}

/// Record a live install/verification check (Ollama digest/tag, timestamp)
/// -- local providers only. Never a trusted cache: callers must call this
/// only right after an actual live check, matching
/// get_capability_profile's own live-check-never-cached convention.
pub async fn record_local_install(
    pool: &sqlx::SqlitePool,
    user_id: &str,
    provider_id: &str,
    local_model_version: &str,
) -> Result<(), UserProviderPreferenceStoreError> {
    let now = crate::providers::utils::now();
    let mut conn = pool.acquire().await?;

    let result = sqlx::query(
        "UPDATE user_provider_preference
         SET local_model_version = ?, installed_at = COALESCE(installed_at, ?),
             last_verified_at = ?
         WHERE user_id = ? AND persona_id IS NULL AND focus_id IS NULL AND provider_id = ?",
    )
    .bind(local_model_version)
    .bind(&now)
    .bind(&now)
    .bind(user_id)
    .bind(provider_id)
    .execute(&mut *conn)
    .await?;

    if result.rows_affected() == 0 {
        return Err(UserProviderPreferenceStoreError::NotFound(format!(
            "{user_id}/{provider_id}"
        )));
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
    use sqlx::ConnectOptions;

    /// Sets QR_DATA_ROOT to a fresh tempdir, migrates shared.db for real,
    /// and seeds a user + persona -- so tests below can call the actual
    /// public API (resolve_preference, upsert_preference), which opens its
    /// own connection via pool.acquire()/get_data_root(), not an
    /// in-memory fixture. Matches migrations.rs's own QR_DATA_ROOT-mutating
    /// test pattern (ENV_MUTEX serialization). Returns the tempdir so it
    /// isn't dropped (and deleted) before the calling test finishes.
    async fn setup_real_db() -> (tempfile::TempDir, sqlx::SqlitePool) {
        let tempdir = tempfile::tempdir().expect("failed to create tempdir");
        std::env::set_var("QR_DATA_ROOT", tempdir.path());

        crate::persistence::migrations::migrate_shared_db()
            .await
            .expect("migrate_shared_db must succeed");

        let db_path = tempdir.path().join("instance").join("shared.db");
        let mut conn = SqliteConnectOptions::new()
            .filename(&db_path)
            .connect()
            .await
            .expect("open seeded shared.db");

        sqlx::query(
            "INSERT INTO users (id, display_name, role, is_primary, auth_enabled, created_at) \
             VALUES ('u1', 'Test User', 'user', 1, 1, '2026-01-01T00:00:00Z')",
        )
        .execute(&mut conn)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO personas (id, display_name, persona_type, created_at) \
             VALUES ('p1', 'Personal', 'personal', '2026-01-01T00:00:00Z')",
        )
        .execute(&mut conn)
        .await
        .unwrap();

        let pool =
            sqlx::SqlitePool::connect_with(crate::providers::utils::connect_options_unencrypted(
                &crate::providers::utils::db_path_shared(),
            ))
            .await
            .expect("shared.db pool must connect");

        (tempdir, pool)
    }

    /// items.id=428: most-specific-match-wins -- a Focus row must win over
    /// a Persona row, which must win over an account-wide row, exercised
    /// via the real resolve_preference() public API.
    #[tokio::test]
    async fn resolve_preference_focus_beats_persona_beats_account() {
        let _lock = crate::test_support::ENV_MUTEX.lock().await;
        let saved_root = std::env::var("QR_DATA_ROOT").ok();
        let (_tempdir, pool) = setup_real_db().await;

        let outcome = async {
            upsert_preference(
                &pool,
                NewUserProviderPreference {
                    user_id: "u1",
                    persona_id: None,
                    focus_id: None,
                    provider_id: "duckai",
                    login_available: false,
                    user_preference: UserPreference::Allowed,
                    local_model_version: None,
                    subscription_status: None,
                    plan_key: None,
                    model_id: None,
                },
            )
            .await?;
            upsert_preference(
                &pool,
                NewUserProviderPreference {
                    user_id: "u1",
                    persona_id: Some("p1"),
                    focus_id: None,
                    provider_id: "duckai",
                    login_available: false,
                    user_preference: UserPreference::Preferred,
                    local_model_version: None,
                    subscription_status: None,
                    plan_key: None,
                    model_id: None,
                },
            )
            .await?;
            upsert_preference(
                &pool,
                NewUserProviderPreference {
                    user_id: "u1",
                    persona_id: Some("p1"),
                    focus_id: Some("f1"),
                    provider_id: "duckai",
                    login_available: false,
                    user_preference: UserPreference::Declined,
                    local_model_version: None,
                    subscription_status: None,
                    plan_key: None,
                    model_id: None,
                },
            )
            .await?;

            let focus_scoped =
                resolve_preference(&pool, "u1", Some("p1"), Some("f1"), "duckai").await?;
            assert_eq!(
                focus_scoped.unwrap().user_preference,
                UserPreference::Declined,
                "the Focus-scoped row must win when a focus_id is given"
            );

            let persona_scoped =
                resolve_preference(&pool, "u1", Some("p1"), None, "duckai").await?;
            assert_eq!(
                persona_scoped.unwrap().user_preference,
                UserPreference::Preferred,
                "the Persona-scoped row must win when no focus_id is given"
            );

            let account_scoped = resolve_preference(&pool, "u1", None, None, "duckai").await?;
            assert_eq!(
                account_scoped.unwrap().user_preference,
                UserPreference::Allowed,
                "the account-wide row must win when neither persona_id nor focus_id is given"
            );

            // A focus_id belonging to a *different*, override-free persona
            // scope must NOT see the p1/f1 override -- falls through focus
            // (no match under a different persona_id combination isn't
            // queried here) to persona p1's own row.
            let persona_p1_no_focus_override =
                resolve_preference(&pool, "u1", Some("p1"), Some("f-unrelated"), "duckai").await?;
            assert_eq!(
                persona_p1_no_focus_override.unwrap().user_preference,
                UserPreference::Preferred,
                "an unrelated focus_id under the same persona must fall back to the persona row"
            );

            Ok::<(), UserProviderPreferenceStoreError>(())
        }
        .await;

        if let Some(v) = saved_root {
            std::env::set_var("QR_DATA_ROOT", v);
        } else {
            std::env::remove_var("QR_DATA_ROOT");
        }

        outcome.expect("precedence assertions must pass");
    }

    /// No row at any scope -- resolve_preference() must return None so the
    /// caller applies its own system-wide hardcoded default, never
    /// inheriting from another user (Part 2b's explicit new-user
    /// convention).
    #[tokio::test]
    async fn no_row_at_any_scope_means_no_match() {
        let _lock = crate::test_support::ENV_MUTEX.lock().await;
        let saved_root = std::env::var("QR_DATA_ROOT").ok();
        let (_tempdir, pool) = setup_real_db().await;

        let result = resolve_preference(&pool, "u1", Some("p1"), Some("f1"), "duckai").await;

        if let Some(v) = saved_root {
            std::env::set_var("QR_DATA_ROOT", v);
        } else {
            std::env::remove_var("QR_DATA_ROOT");
        }

        assert!(result.expect("query must succeed").is_none());
    }

    /// items.id=432: zero/one/many-candidates cases for
    /// find_preferred_provider -- this is the function lifecycle.rs uses to
    /// choose between hosted providers instead of resolving one named provider.
    #[tokio::test]
    async fn find_preferred_provider_zero_one_many_candidates() {
        let _lock = crate::test_support::ENV_MUTEX.lock().await;
        let saved_root = std::env::var("QR_DATA_ROOT").ok();
        let (_tempdir, pool) = setup_real_db().await;

        let outcome = async {
            // The hosted providers rows are seeded by the shared_*.sql
            // migrations (items.id=429/430/432, 724), applied as part of
            // setup_real_db()'s migrate_shared_db() call above. Two are
            // chosen by field so this test names no provider.
            let hosted: Vec<(String,)> = sqlx::query_as(
                "SELECT id FROM providers WHERE provider_type = 'cloud_inference_api' \
                 ORDER BY id LIMIT 2",
            )
            .fetch_all(&pool)
            .await?;
            let first = hosted[0].0.clone();
            let second = hosted[1].0.clone();
            let candidates = vec![first.clone(), second.clone()];

            // Zero: neither candidate has any row at all.
            let none = find_preferred_provider(&pool, "u1", Some("p1"), None, &candidates).await?;
            assert_eq!(none, None, "no preference set for either candidate -> None");

            // One: the first candidate marked Preferred, the second untouched.
            upsert_preference(
                &pool,
                NewUserProviderPreference {
                    user_id: "u1",
                    persona_id: None,
                    focus_id: None,
                    provider_id: &first,
                    login_available: true,
                    user_preference: UserPreference::Preferred,
                    local_model_version: None,
                    subscription_status: None,
                    plan_key: None,
                    model_id: None,
                },
            )
            .await?;
            let one = find_preferred_provider(&pool, "u1", Some("p1"), None, &candidates).await?;
            assert_eq!(one, Some(first.clone()));

            // Many: the second candidate ALSO marked Preferred (a data anomaly the schema
            // doesn't prevent) -> ambiguous, must return None rather than
            // guess, same as this codebase's unreachable!()-guarded pattern.
            upsert_preference(
                &pool,
                NewUserProviderPreference {
                    user_id: "u1",
                    persona_id: None,
                    focus_id: None,
                    provider_id: &second,
                    login_available: true,
                    user_preference: UserPreference::Preferred,
                    local_model_version: None,
                    subscription_status: None,
                    plan_key: None,
                    model_id: None,
                },
            )
            .await?;
            let many = find_preferred_provider(&pool, "u1", Some("p1"), None, &candidates).await?;
            assert_eq!(
                many, None,
                "two candidates both Preferred is ambiguous -> None"
            );

            Ok::<(), UserProviderPreferenceStoreError>(())
        }
        .await;

        if let Some(v) = saved_root {
            std::env::set_var("QR_DATA_ROOT", v);
        } else {
            std::env::remove_var("QR_DATA_ROOT");
        }
        outcome.expect("find_preferred_provider assertions must pass");
    }

    /// The three partial unique indexes must actually reject a second
    /// account-wide row for the same (user_id, provider_id) -- this is the
    /// concrete enforcement judgment call 2 in this session's plan rests on.
    #[tokio::test]
    async fn duplicate_account_wide_row_is_rejected() {
        let _lock = crate::test_support::ENV_MUTEX.lock().await;
        let saved_root = std::env::var("QR_DATA_ROOT").ok();
        let (tempdir, _pool) = setup_real_db().await;

        let db_path = tempdir.path().join("instance").join("shared.db");
        let outcome = async {
            let mut conn = SqliteConnectOptions::new()
                .filename(&db_path)
                .connect()
                .await
                .expect("open seeded shared.db");

            sqlx::query(
                "INSERT INTO user_provider_preference
                 (id, user_id, persona_id, focus_id, provider_id, login_available,
                  user_preference, created_at)
                 VALUES (?, 'u1', NULL, NULL, 'duckai', 0, 'allowed', '2026-01-01T00:00:00Z')",
            )
            .bind(uuid::Uuid::new_v4().to_string())
            .execute(&mut conn)
            .await
            .expect("first account-wide row must insert cleanly");

            sqlx::query(
                "INSERT INTO user_provider_preference
                 (id, user_id, persona_id, focus_id, provider_id, login_available,
                  user_preference, created_at)
                 VALUES (?, 'u1', NULL, NULL, 'duckai', 0, 'declined', '2026-01-01T00:00:00Z')",
            )
            .bind(uuid::Uuid::new_v4().to_string())
            .execute(&mut conn)
            .await
        }
        .await;

        if let Some(v) = saved_root {
            std::env::set_var("QR_DATA_ROOT", v);
        } else {
            std::env::remove_var("QR_DATA_ROOT");
        }

        assert!(
            outcome.is_err(),
            "a second account-wide row for the same user+provider must violate \
             idx_user_provider_pref_account"
        );
    }

    /// items.id=708: plan_key and model_id round-trip through both upsert
    /// paths (insert, then update) and the Focus > Persona > account read.
    #[tokio::test]
    async fn plan_key_and_model_id_round_trip() {
        let _lock = crate::test_support::ENV_MUTEX.lock().await;
        let saved_root = std::env::var("QR_DATA_ROOT").ok();
        let (_tempdir, pool) = setup_real_db().await;

        let outcome = async {
            let inserted = upsert_preference(
                &pool,
                NewUserProviderPreference {
                    user_id: "u1",
                    persona_id: None,
                    focus_id: None,
                    provider_id: "duckai",
                    login_available: true,
                    user_preference: UserPreference::Allowed,
                    local_model_version: None,
                    subscription_status: None,
                    plan_key: Some("fixture-plan"),
                    model_id: Some("fixture-model-a"),
                },
            )
            .await?;
            assert_eq!(inserted.plan_key.as_deref(), Some("fixture-plan"));
            assert_eq!(inserted.model_id.as_deref(), Some("fixture-model-a"));

            let read = resolve_preference(&pool, "u1", None, None, "duckai")
                .await?
                .expect("row exists");
            assert_eq!(read.plan_key.as_deref(), Some("fixture-plan"));
            assert_eq!(read.model_id.as_deref(), Some("fixture-model-a"));

            let updated = upsert_preference(
                &pool,
                NewUserProviderPreference {
                    user_id: "u1",
                    persona_id: None,
                    focus_id: None,
                    provider_id: "duckai",
                    login_available: true,
                    user_preference: UserPreference::Allowed,
                    local_model_version: None,
                    subscription_status: None,
                    plan_key: None,
                    model_id: Some("fixture-model-b"),
                },
            )
            .await?;
            assert_eq!(updated.plan_key, None);
            assert_eq!(updated.model_id.as_deref(), Some("fixture-model-b"));
            Ok::<_, UserProviderPreferenceStoreError>(())
        }
        .await;

        if let Some(v) = saved_root {
            std::env::set_var("QR_DATA_ROOT", v);
        } else {
            std::env::remove_var("QR_DATA_ROOT");
        }
        outcome.expect("plan_key and model_id must round-trip");
    }

    // -- items.id=711: model selection ---------------------------------------

    async fn pref_of(
        pool: &sqlx::SqlitePool,
        persona: Option<&str>,
        focus: Option<&str>,
        provider: &str,
    ) -> Option<UserProviderPreference> {
        get_preference_at_scope(pool, "u1", persona, focus, provider)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn setting_a_model_on_an_existing_row_changes_only_the_model() {
        let _lock = crate::test_support::ENV_MUTEX.lock().await;
        let saved_root = std::env::var("QR_DATA_ROOT").ok();
        let (_tempdir, pool) = setup_real_db().await;

        let outcome = async {
            upsert_preference(
                &pool,
                NewUserProviderPreference {
                    user_id: "u1",
                    persona_id: None,
                    focus_id: None,
                    provider_id: "duckai",
                    login_available: true,
                    user_preference: UserPreference::Preferred,
                    local_model_version: Some("v1"),
                    subscription_status: Some(SubscriptionStatus::Paid),
                    plan_key: Some("plan-x"),
                    model_id: None,
                },
            )
            .await?;
            set_model_selection(
                &pool,
                "u1",
                &SelectionScope::Account,
                None,
                "duckai",
                Some("m-1"),
            )
            .await?;
            let row = pref_of(&pool, None, None, "duckai").await.unwrap();
            assert_eq!(row.model_id.as_deref(), Some("m-1"));
            assert_eq!(row.user_preference, UserPreference::Preferred);
            assert!(row.login_available);
            assert_eq!(row.local_model_version.as_deref(), Some("v1"));
            assert_eq!(row.subscription_status, Some(SubscriptionStatus::Paid));
            assert_eq!(row.plan_key.as_deref(), Some("plan-x"));

            // Clearing leaves the row and its preference.
            set_model_selection(&pool, "u1", &SelectionScope::Account, None, "duckai", None)
                .await?;
            let row = pref_of(&pool, None, None, "duckai").await.unwrap();
            assert_eq!(row.model_id, None);
            assert_eq!(row.user_preference, UserPreference::Preferred);
            Ok::<(), UserProviderPreferenceStoreError>(())
        }
        .await;

        match saved_root {
            Some(v) => std::env::set_var("QR_DATA_ROOT", v),
            None => std::env::remove_var("QR_DATA_ROOT"),
        }
        outcome.expect("model selection must succeed");
    }

    #[tokio::test]
    async fn a_new_narrower_row_copies_the_resolved_preference_or_is_allowed() {
        let _lock = crate::test_support::ENV_MUTEX.lock().await;
        let saved_root = std::env::var("QR_DATA_ROOT").ok();
        let (_tempdir, pool) = setup_real_db().await;

        let outcome = async {
            upsert_preference(
                &pool,
                NewUserProviderPreference {
                    user_id: "u1",
                    persona_id: None,
                    focus_id: None,
                    provider_id: "duckai",
                    login_available: true,
                    user_preference: UserPreference::Preferred,
                    local_model_version: None,
                    subscription_status: None,
                    plan_key: Some("plan-x"),
                    model_id: Some("account-model"),
                },
            )
            .await?;
            let persona = SelectionScope::Persona {
                persona_id: "p1".into(),
            };
            set_model_selection(&pool, "u1", &persona, None, "duckai", Some("persona-model"))
                .await?;
            let row = pref_of(&pool, Some("p1"), None, "duckai").await.unwrap();
            assert_eq!(row.model_id.as_deref(), Some("persona-model"));
            assert_eq!(row.user_preference, UserPreference::Preferred, "copied");
            assert!(row.login_available);
            assert_eq!(row.plan_key.as_deref(), Some("plan-x"));
            // The account row is untouched.
            let account = pref_of(&pool, None, None, "duckai").await.unwrap();
            assert_eq!(account.model_id.as_deref(), Some("account-model"));

            // A provider with no row anywhere: allowed, never Preferred.
            set_model_selection(&pool, "u1", &persona, None, "claude", Some("m")).await?;
            let row = pref_of(&pool, Some("p1"), None, "claude").await.unwrap();
            assert_eq!(row.user_preference, UserPreference::Allowed);

            // Clearing where no row exists writes nothing.
            set_model_selection(
                &pool,
                "u1",
                &SelectionScope::Focus {
                    focus_id: "f9".into(),
                },
                Some("p1"),
                "duckai",
                None,
            )
            .await?;
            assert!(pref_of(&pool, None, Some("f9"), "duckai").await.is_none());

            // A Focus row copies what the Focus resolves to with its Persona.
            set_model_selection(
                &pool,
                "u1",
                &SelectionScope::Focus {
                    focus_id: "f1".into(),
                },
                Some("p1"),
                "duckai",
                Some("focus-model"),
            )
            .await?;
            let row = pref_of(&pool, None, Some("f1"), "duckai").await.unwrap();
            assert_eq!(row.user_preference, UserPreference::Preferred);
            let resolved =
                resolve_model_selection(&pool, "u1", Some("p1"), Some("f1"), "duckai").await?;
            assert_eq!(
                resolved,
                Some(ResolvedModelSelection {
                    model_id: "focus-model".into(),
                    source: SelectionSource::Focus
                })
            );
            Ok::<(), UserProviderPreferenceStoreError>(())
        }
        .await;

        match saved_root {
            Some(v) => std::env::set_var("QR_DATA_ROOT", v),
            None => std::env::remove_var("QR_DATA_ROOT"),
        }
        outcome.expect("model selection must succeed");
    }

    #[tokio::test]
    async fn an_empty_scope_id_is_refused() {
        let _lock = crate::test_support::ENV_MUTEX.lock().await;
        let saved_root = std::env::var("QR_DATA_ROOT").ok();
        let (_tempdir, pool) = setup_real_db().await;
        let err = set_model_selection(
            &pool,
            "u1",
            &SelectionScope::Persona {
                persona_id: String::new(),
            },
            None,
            "duckai",
            Some("m"),
        )
        .await;
        match saved_root {
            Some(v) => std::env::set_var("QR_DATA_ROOT", v),
            None => std::env::remove_var("QR_DATA_ROOT"),
        }
        assert!(matches!(
            err,
            Err(UserProviderPreferenceStoreError::Validation(_))
        ));
    }
}
