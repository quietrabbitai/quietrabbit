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

use sqlx::sqlite::SqliteConnectOptions;
use sqlx::ConnectOptions;
use sqlx::Row;
use sqlx::SqliteConnection;
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
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
}

// ---------------------------------------------------------------------------
// DB opener (shared.db — unencrypted)
// ---------------------------------------------------------------------------

async fn open_shared_db() -> Result<SqliteConnection, UserProviderPreferenceStoreError> {
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

const SELECT_COLUMNS: &str = "id, user_id, persona_id, focus_id, provider_id,
                login_available, user_preference, enabled_at, declined_at,
                local_model_version, installed_at, last_verified_at,
                subscription_status, created_at";

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
    user_id: &str,
    persona_id: Option<&str>,
    focus_id: Option<&str>,
    provider_id: &str,
) -> Result<Option<UserProviderPreference>, UserProviderPreferenceStoreError> {
    let mut conn = open_shared_db().await?;

    if let Some(focus_id) = focus_id {
        let sql = format!(
            "SELECT {SELECT_COLUMNS} FROM user_provider_preference
             WHERE user_id = ? AND focus_id = ? AND provider_id = ?"
        );
        if let Some(row) = sqlx::query(&sql)
            .bind(user_id)
            .bind(focus_id)
            .bind(provider_id)
            .fetch_optional(&mut conn)
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
            .fetch_optional(&mut conn)
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
        .fetch_optional(&mut conn)
        .await?;

    match row {
        None => Ok(None),
        Some(r) => Ok(Some(row_to_preference(&r)?)),
    }
}

/// items.id=432: picks the Preferred provider across several candidates
/// (e.g. the Tier 1.5 set -- resolve_preference() itself only answers "what
/// is the preference for this ONE provider_id," but lifecycle.rs needs to
/// choose BETWEEN groq and mistral, not resolve one named provider).
/// Resolves each candidate independently at the given scope and returns the
/// single one whose resolved preference is Preferred. Zero matches -> None,
/// matching this codebase's existing "no prescribed default" behavior
/// (surfaces MissingTier2Config downstream, same as today). More than one
/// match is a data anomaly the schema doesn't prevent (nothing stops two
/// different providers both being marked Preferred at the same scope) --
/// also returns None rather than an arbitrary pick, consistent with this
/// codebase's existing unreachable!()-guarded "never silently guess"
/// pattern in executor.rs.
pub async fn find_preferred_provider(
    user_id: &str,
    persona_id: Option<&str>,
    focus_id: Option<&str>,
    candidate_provider_ids: &[String],
) -> Result<Option<String>, UserProviderPreferenceStoreError> {
    let mut preferred: Vec<String> = Vec::new();
    for provider_id in candidate_provider_ids {
        if let Some(pref) = resolve_preference(user_id, persona_id, focus_id, provider_id).await? {
            if pref.user_preference == UserPreference::Preferred {
                preferred.push(provider_id.clone());
            }
        }
    }

    match preferred.len() {
        1 => Ok(Some(preferred.into_iter().next().unwrap())),
        _ => Ok(None),
    }
}

/// Raw lookup at an exact scope (no cascading) -- for callers that need to
/// know whether a specific-scope override row exists, distinct from
/// resolve_preference()'s cascading result.
pub async fn get_preference_at_scope(
    user_id: &str,
    persona_id: Option<&str>,
    focus_id: Option<&str>,
    provider_id: &str,
) -> Result<Option<UserProviderPreference>, UserProviderPreferenceStoreError> {
    let mut conn = open_shared_db().await?;

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
        .fetch_optional(&mut conn)
        .await?;

    match row {
        None => Ok(None),
        Some(r) => Ok(Some(row_to_preference(&r)?)),
    }
}

/// All preference rows for a user, across every scope -- for a future
/// settings-surface listing, not a hot read path.
pub async fn list_preferences_for_user(
    user_id: &str,
) -> Result<Vec<UserProviderPreference>, UserProviderPreferenceStoreError> {
    let mut conn = open_shared_db().await?;

    let sql = format!(
        "SELECT {SELECT_COLUMNS} FROM user_provider_preference
         WHERE user_id = ?
         ORDER BY provider_id ASC, persona_id ASC, focus_id ASC"
    );
    let rows = sqlx::query(&sql).bind(user_id).fetch_all(&mut conn).await?;

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
    new: NewUserProviderPreference<'_>,
) -> Result<UserProviderPreference, UserProviderPreferenceStoreError> {
    let existing =
        get_preference_at_scope(new.user_id, new.persona_id, new.focus_id, new.provider_id).await?;

    let now = crate::providers::utils::now();
    let mut conn = open_shared_db().await?;

    if let Some(existing) = existing {
        sqlx::query(
            "UPDATE user_provider_preference
             SET login_available = ?, user_preference = ?, local_model_version = ?,
                 subscription_status = ?,
                 enabled_at = CASE WHEN ? = 'declined' THEN enabled_at ELSE ? END,
                 declined_at = CASE WHEN ? = 'declined' THEN ? ELSE declined_at END
             WHERE id = ?",
        )
        .bind(new.login_available as i64)
        .bind(new.user_preference.as_str())
        .bind(new.local_model_version)
        .bind(new.subscription_status.map(|s| s.as_str()))
        .bind(new.user_preference.as_str())
        .bind(&now)
        .bind(new.user_preference.as_str())
        .bind(&now)
        .bind(&existing.id)
        .execute(&mut conn)
        .await?;

        return get_preference_at_scope(new.user_id, new.persona_id, new.focus_id, new.provider_id)
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
          installed_at, last_verified_at, subscription_status, created_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NULL, NULL, ?, ?)",
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
    .execute(&mut conn)
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
    })
}

/// Record a live install/verification check (Ollama digest/tag, timestamp)
/// -- local providers only. Never a trusted cache: callers must call this
/// only right after an actual live check, matching
/// get_capability_profile's own live-check-never-cached convention.
pub async fn record_local_install(
    user_id: &str,
    provider_id: &str,
    local_model_version: &str,
) -> Result<(), UserProviderPreferenceStoreError> {
    let now = crate::providers::utils::now();
    let mut conn = open_shared_db().await?;

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
    .execute(&mut conn)
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

    /// Sets QR_DATA_ROOT to a fresh tempdir, migrates shared.db for real,
    /// and seeds a user + persona -- so tests below can call the actual
    /// public API (resolve_preference, upsert_preference), which opens its
    /// own connection via open_shared_db()/get_data_root(), not an
    /// in-memory fixture. Matches migrations.rs's own QR_DATA_ROOT-mutating
    /// test pattern (ENV_MUTEX serialization). Returns the tempdir so it
    /// isn't dropped (and deleted) before the calling test finishes.
    async fn setup_real_db() -> tempfile::TempDir {
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

        tempdir
    }

    /// items.id=428: most-specific-match-wins -- a Focus row must win over
    /// a Persona row, which must win over an account-wide row, exercised
    /// via the real resolve_preference() public API.
    #[tokio::test]
    async fn resolve_preference_focus_beats_persona_beats_account() {
        let _lock = crate::test_support::ENV_MUTEX.lock().unwrap();
        let saved_root = std::env::var("QR_DATA_ROOT").ok();
        let _tempdir = setup_real_db().await;

        let outcome = async {
            upsert_preference(NewUserProviderPreference {
                user_id: "u1",
                persona_id: None,
                focus_id: None,
                provider_id: "duckai",
                login_available: false,
                user_preference: UserPreference::Allowed,
                local_model_version: None,
                subscription_status: None,
            })
            .await?;
            upsert_preference(NewUserProviderPreference {
                user_id: "u1",
                persona_id: Some("p1"),
                focus_id: None,
                provider_id: "duckai",
                login_available: false,
                user_preference: UserPreference::Preferred,
                local_model_version: None,
                subscription_status: None,
            })
            .await?;
            upsert_preference(NewUserProviderPreference {
                user_id: "u1",
                persona_id: Some("p1"),
                focus_id: Some("f1"),
                provider_id: "duckai",
                login_available: false,
                user_preference: UserPreference::Declined,
                local_model_version: None,
                subscription_status: None,
            })
            .await?;

            let focus_scoped = resolve_preference("u1", Some("p1"), Some("f1"), "duckai").await?;
            assert_eq!(
                focus_scoped.unwrap().user_preference,
                UserPreference::Declined,
                "the Focus-scoped row must win when a focus_id is given"
            );

            let persona_scoped = resolve_preference("u1", Some("p1"), None, "duckai").await?;
            assert_eq!(
                persona_scoped.unwrap().user_preference,
                UserPreference::Preferred,
                "the Persona-scoped row must win when no focus_id is given"
            );

            let account_scoped = resolve_preference("u1", None, None, "duckai").await?;
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
                resolve_preference("u1", Some("p1"), Some("f-unrelated"), "duckai").await?;
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
        let _lock = crate::test_support::ENV_MUTEX.lock().unwrap();
        let saved_root = std::env::var("QR_DATA_ROOT").ok();
        let _tempdir = setup_real_db().await;

        let result = resolve_preference("u1", Some("p1"), Some("f1"), "duckai").await;

        if let Some(v) = saved_root {
            std::env::set_var("QR_DATA_ROOT", v);
        } else {
            std::env::remove_var("QR_DATA_ROOT");
        }

        assert!(result.expect("query must succeed").is_none());
    }

    /// items.id=432: zero/one/many-candidates cases for
    /// find_preferred_provider -- this is the function lifecycle.rs uses to
    /// choose between groq/mistral instead of resolving one named provider.
    #[tokio::test]
    async fn find_preferred_provider_zero_one_many_candidates() {
        let _lock = crate::test_support::ENV_MUTEX.lock().unwrap();
        let saved_root = std::env::var("QR_DATA_ROOT").ok();
        let _tempdir = setup_real_db().await;

        let outcome = async {
            // groq/mistral already exist as providers rows -- seeded by
            // shared_014.sql (items.id=429/430/432), applied as part of
            // setup_real_db()'s migrate_shared_db() call above.
            let candidates = vec!["groq".to_owned(), "mistral".to_owned()];

            // Zero: neither candidate has any row at all.
            let none = find_preferred_provider("u1", Some("p1"), None, &candidates).await?;
            assert_eq!(none, None, "no preference set for either candidate -> None");

            // One: groq marked Preferred, mistral untouched.
            upsert_preference(NewUserProviderPreference {
                user_id: "u1",
                persona_id: None,
                focus_id: None,
                provider_id: "groq",
                login_available: true,
                user_preference: UserPreference::Preferred,
                local_model_version: None,
                subscription_status: None,
            })
            .await?;
            let one = find_preferred_provider("u1", Some("p1"), None, &candidates).await?;
            assert_eq!(one, Some("groq".to_owned()));

            // Many: mistral ALSO marked Preferred (a data anomaly the schema
            // doesn't prevent) -> ambiguous, must return None rather than
            // guess, same as this codebase's unreachable!()-guarded pattern.
            upsert_preference(NewUserProviderPreference {
                user_id: "u1",
                persona_id: None,
                focus_id: None,
                provider_id: "mistral",
                login_available: true,
                user_preference: UserPreference::Preferred,
                local_model_version: None,
                subscription_status: None,
            })
            .await?;
            let many = find_preferred_provider("u1", Some("p1"), None, &candidates).await?;
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
        let _lock = crate::test_support::ENV_MUTEX.lock().unwrap();
        let saved_root = std::env::var("QR_DATA_ROOT").ok();
        let tempdir = setup_real_db().await;

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
}
