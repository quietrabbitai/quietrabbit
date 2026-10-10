// src-tauri/src/persistence/focus_provider_criteria_store.rs
//
// focus_provider_criteria CRUD for shared.db (unencrypted) -- items.id=429
// (PROVIDER_REGISTRY_AND_TIER_MODEL_SPEC.md Part 3c). One row per Focus:
// require_* flags (AND-composed), an explicit allow/deny provider-id list,
// and seeded_from_policy (display/reset metadata only -- see below). See
// shared_014.sql's own focus_provider_criteria header for full
// column-by-column rationale; not re-derived here.
//
// SCOPE: the focus_provider_criteria table and eligible_providers_for_focus()
// are still ADDITIVE, parallel infrastructure -- nothing in the live
// execution path reads that table yet (no code creates rows for a Focus, and
// the rows are not kept in sync with the live ceiling). The live Focus
// ceiling is focus_settings.max_permitted_tier (an ExternalAccess), and its
// anonymity rule IS enforced at provider selection (items.id=692,
// decisions.id=870) through providers_permitted_by_ceiling() below, which
// conductor::provider_selection applies before the user's provider
// preference is resolved. No IPC command surface is exposed (matches
// provider_store.rs / user_provider_preference_store.rs's own "CRUD now, IPC
// when a real caller needs one" precedent -- Focus Builder UI, spec Part 5b,
// is separate future work). When the criteria table is wired in, it composes
// with the ceiling as an additional AND filter; it does not replace it.
//
// NAMED POLICIES ARE TEMPLATES, NEVER RE-READ BY ENFORCEMENT: selecting a
// policy populates this table's require_* flags once; seeded_from_policy is
// retained purely for display ("based on Local Only") and to support
// reset_to_policy(). This is the explicit cautionary precedent the spec
// names: focus_settings.focus_profile (items.id=230) was originally
// documented as pure shorthand but its own enforcement code
// (commands/library.rs) reads it independently, and an earlier version of
// that same code's own comment conflated the label with unrelated
// enforcement. This design does not repeat that: eligible_providers_for_focus()
// below reads ONLY require_is_local/require_is_anonymous/
// require_not_trains_on_data/allow_provider_ids/deny_provider_ids -- never
// seeded_from_policy.
//
// JUDGMENT CALL 2 (this session, revised after Jason's correction): require_*
// flags are AND-composed -- "a provider must satisfy every set-true
// requirement" (spec's own words). local_and_anonymous therefore sets BOTH
// require_is_local AND require_is_anonymous on the same row, not a
// single-flag shortcut relying on any is_local-implies-is_anonymous
// convention -- a future non-anonymous local-model provider row must
// correctly fail local_and_anonymous while still passing local_only.
//
// QUERY STYLE / CONNECTION MODEL: matches provider_store.rs /
// user_provider_preference_store.rs exactly -- runtime sqlx::query() only,
// one connection per call, shared.db (unencrypted, no PRAGMA key required).

use sqlx::Row;
use thiserror::Error;

use crate::conductor::tokens::ExternalAccess;
use crate::persistence::provider_store::{self, Provider, ProviderStoreError};

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum FocusProviderCriteriaStoreError {
    #[error("Database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("Criteria row for focus '{0}' not found")]
    NotFound(String),
    #[error("Validation error: {0}")]
    Validation(String),
    #[error("Provider store error: {0}")]
    ProviderStore(#[from] ProviderStoreError),
}

// ---------------------------------------------------------------------------
// Data types
// ---------------------------------------------------------------------------

/// Focus-Builder-time templates (spec Part 3c) -- populate a criteria row
/// once, never read back by enforcement. See judgment call 2 above for why
/// LocalAndAnonymous sets two flags, not one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NamedPolicy {
    LocalOnly,
    LocalAndAnonymous,
    NoTrainingDefault,
    Unrestricted,
}

impl NamedPolicy {
    fn as_str(self) -> &'static str {
        match self {
            NamedPolicy::LocalOnly => "local_only",
            NamedPolicy::LocalAndAnonymous => "local_and_anonymous",
            NamedPolicy::NoTrainingDefault => "no_training_default",
            NamedPolicy::Unrestricted => "unrestricted",
        }
    }

    fn from_str(s: &str) -> Result<Self, FocusProviderCriteriaStoreError> {
        match s {
            "local_only" => Ok(NamedPolicy::LocalOnly),
            "local_and_anonymous" => Ok(NamedPolicy::LocalAndAnonymous),
            "no_training_default" => Ok(NamedPolicy::NoTrainingDefault),
            "unrestricted" => Ok(NamedPolicy::Unrestricted),
            other => Err(FocusProviderCriteriaStoreError::Validation(format!(
                "seeded_from_policy must be 'local_only', 'local_and_anonymous', \
                 'no_training_default', or 'unrestricted', got '{other}' -- schema CHECK \
                 should have rejected this at write time."
            ))),
        }
    }

    /// (require_is_local, require_is_anonymous, require_not_trains_on_data)
    /// this policy populates. Judgment call 2: LocalAndAnonymous is a
    /// genuine two-flag AND, not a single-flag shortcut.
    fn require_flags(self) -> (bool, bool, bool) {
        match self {
            NamedPolicy::LocalOnly => (true, false, false),
            NamedPolicy::LocalAndAnonymous => (true, true, false),
            NamedPolicy::NoTrainingDefault => (false, false, true),
            NamedPolicy::Unrestricted => (false, false, false),
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct FocusProviderCriteria {
    pub focus_id: String,
    pub require_is_local: bool,
    pub require_is_anonymous: bool,
    pub require_not_trains_on_data: bool,
    pub allow_provider_ids: Vec<String>,
    pub deny_provider_ids: Vec<String>,
    /// Display/reset metadata only -- see module header. Never read by
    /// eligible_providers_for_focus().
    pub seeded_from_policy: Option<NamedPolicy>,
    pub created_at: String,
    pub updated_at: String,
}

// ---------------------------------------------------------------------------
// DB opener (shared.db — unencrypted)
// ---------------------------------------------------------------------------

const SELECT_COLUMNS: &str = "focus_id, require_is_local, require_is_anonymous,
                require_not_trains_on_data, allow_provider_ids, deny_provider_ids,
                seeded_from_policy, created_at, updated_at";

fn parse_id_list(raw: &str) -> Result<Vec<String>, FocusProviderCriteriaStoreError> {
    serde_json::from_str(raw).map_err(|e| {
        FocusProviderCriteriaStoreError::Validation(format!(
            "allow/deny_provider_ids not valid JSON array: {e}"
        ))
    })
}

fn row_to_criteria(
    row: &sqlx::sqlite::SqliteRow,
) -> Result<FocusProviderCriteria, FocusProviderCriteriaStoreError> {
    let require_is_local_raw: i64 = row.try_get("require_is_local")?;
    let require_is_anonymous_raw: i64 = row.try_get("require_is_anonymous")?;
    let require_not_trains_raw: i64 = row.try_get("require_not_trains_on_data")?;
    let allow_raw: String = row.try_get("allow_provider_ids")?;
    let deny_raw: String = row.try_get("deny_provider_ids")?;
    let seeded_from_policy_raw: Option<String> = row.try_get("seeded_from_policy")?;

    let seeded_from_policy = seeded_from_policy_raw
        .as_deref()
        .map(NamedPolicy::from_str)
        .transpose()?;

    Ok(FocusProviderCriteria {
        focus_id: row.try_get("focus_id")?,
        require_is_local: require_is_local_raw != 0,
        require_is_anonymous: require_is_anonymous_raw != 0,
        require_not_trains_on_data: require_not_trains_raw != 0,
        allow_provider_ids: parse_id_list(&allow_raw)?,
        deny_provider_ids: parse_id_list(&deny_raw)?,
        seeded_from_policy,
        created_at: row.try_get("created_at")?,
        updated_at: row.try_get("updated_at")?,
    })
}

// ---------------------------------------------------------------------------
// Read operations
// ---------------------------------------------------------------------------

pub async fn get_criteria(
    pool: &sqlx::SqlitePool,
    focus_id: &str,
) -> Result<Option<FocusProviderCriteria>, FocusProviderCriteriaStoreError> {
    let mut conn = pool.acquire().await?;
    let sql = format!("SELECT {SELECT_COLUMNS} FROM focus_provider_criteria WHERE focus_id = ?");
    let row = sqlx::query(&sql)
        .bind(focus_id)
        .fetch_optional(&mut *conn)
        .await?;
    match row {
        None => Ok(None),
        Some(r) => Ok(Some(row_to_criteria(&r)?)),
    }
}

// ---------------------------------------------------------------------------
// Write operations
// ---------------------------------------------------------------------------

/// Focus-Builder-time template application (spec Part 3c) -- writes
/// `policy`'s require_* flags, clears allow/deny lists, records
/// seeded_from_policy for display/reset only. Insert-or-replace: applying a
/// policy to a Focus that already has a row overwrites it outright (this is
/// "start over from this policy," not a merge).
pub async fn populate_from_policy(
    pool: &sqlx::SqlitePool,
    focus_id: &str,
    policy: NamedPolicy,
) -> Result<FocusProviderCriteria, FocusProviderCriteriaStoreError> {
    let (require_is_local, require_is_anonymous, require_not_trains_on_data) =
        policy.require_flags();
    write_criteria(
        pool,
        focus_id,
        require_is_local,
        require_is_anonymous,
        require_not_trains_on_data,
        &[],
        &[],
        Some(policy),
    )
    .await
}

/// Re-applies a row's own seeded_from_policy -- the Focus Builder's "reset
/// to policy" action (spec Part 3c). Errors if no row exists, or if the row
/// was never seeded from a policy (nothing to reset to -- e.g. it was built
/// via set_criteria() directly).
pub async fn reset_to_policy(
    pool: &sqlx::SqlitePool,
    focus_id: &str,
) -> Result<FocusProviderCriteria, FocusProviderCriteriaStoreError> {
    let existing = get_criteria(pool, focus_id)
        .await?
        .ok_or_else(|| FocusProviderCriteriaStoreError::NotFound(focus_id.to_owned()))?;
    let policy = existing.seeded_from_policy.ok_or_else(|| {
        FocusProviderCriteriaStoreError::Validation(format!(
            "focus '{focus_id}' has no seeded_from_policy to reset to -- it was built or \
             edited directly via set_criteria(), not from a named policy"
        ))
    })?;
    populate_from_policy(pool, focus_id, policy).await
}

/// Direct edit (Focus Builder's per-Focus override, spec Part 3c) -- clears
/// seeded_from_policy, since an edited record is no longer purely that
/// policy (still resettable if the caller re-applies a policy afterward).
pub async fn set_criteria(
    pool: &sqlx::SqlitePool,
    focus_id: &str,
    require_is_local: bool,
    require_is_anonymous: bool,
    require_not_trains_on_data: bool,
    allow_provider_ids: &[String],
    deny_provider_ids: &[String],
) -> Result<FocusProviderCriteria, FocusProviderCriteriaStoreError> {
    write_criteria(
        pool,
        focus_id,
        require_is_local,
        require_is_anonymous,
        require_not_trains_on_data,
        allow_provider_ids,
        deny_provider_ids,
        None,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn write_criteria(
    pool: &sqlx::SqlitePool,
    focus_id: &str,
    require_is_local: bool,
    require_is_anonymous: bool,
    require_not_trains_on_data: bool,
    allow_provider_ids: &[String],
    deny_provider_ids: &[String],
    seeded_from_policy: Option<NamedPolicy>,
) -> Result<FocusProviderCriteria, FocusProviderCriteriaStoreError> {
    let allow_json = serde_json::to_string(allow_provider_ids).map_err(|e| {
        FocusProviderCriteriaStoreError::Validation(format!(
            "allow_provider_ids not valid JSON: {e}"
        ))
    })?;
    let deny_json = serde_json::to_string(deny_provider_ids).map_err(|e| {
        FocusProviderCriteriaStoreError::Validation(format!(
            "deny_provider_ids not valid JSON: {e}"
        ))
    })?;
    let now = crate::providers::utils::now();
    let mut conn = pool.acquire().await?;

    let existing = get_criteria(pool, focus_id).await?;
    if existing.is_some() {
        sqlx::query(
            "UPDATE focus_provider_criteria
             SET require_is_local = ?, require_is_anonymous = ?, require_not_trains_on_data = ?,
                 allow_provider_ids = ?, deny_provider_ids = ?, seeded_from_policy = ?,
                 updated_at = ?
             WHERE focus_id = ?",
        )
        .bind(require_is_local as i64)
        .bind(require_is_anonymous as i64)
        .bind(require_not_trains_on_data as i64)
        .bind(&allow_json)
        .bind(&deny_json)
        .bind(seeded_from_policy.map(NamedPolicy::as_str))
        .bind(&now)
        .bind(focus_id)
        .execute(&mut *conn)
        .await?;
    } else {
        sqlx::query(
            "INSERT INTO focus_provider_criteria
             (focus_id, require_is_local, require_is_anonymous, require_not_trains_on_data,
              allow_provider_ids, deny_provider_ids, seeded_from_policy, created_at, updated_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(focus_id)
        .bind(require_is_local as i64)
        .bind(require_is_anonymous as i64)
        .bind(require_not_trains_on_data as i64)
        .bind(&allow_json)
        .bind(&deny_json)
        .bind(seeded_from_policy.map(NamedPolicy::as_str))
        .bind(&now)
        .bind(&now)
        .execute(&mut *conn)
        .await?;
    }

    get_criteria(pool, focus_id)
        .await?
        .ok_or_else(|| FocusProviderCriteriaStoreError::NotFound(focus_id.to_owned()))
}

// ---------------------------------------------------------------------------
// Consumption
// ---------------------------------------------------------------------------

/// The criteria-table read path a future consumer would use (spec Part 3c
/// precedence: deny-list excludes unconditionally -> allow-list includes
/// unconditionally -> remainder filtered by require_* flags, AND-composed).
/// No criteria row for `focus_id` -> no restriction configured yet, returns
/// every active provider unfiltered. Not called by the live execution path:
/// provider selection applies the Focus's ExternalAccess ceiling via
/// providers_permitted_by_ceiling() instead (items.id=692).
pub async fn eligible_providers_for_focus(
    pool: &sqlx::SqlitePool,
    focus_id: &str,
) -> Result<Vec<Provider>, FocusProviderCriteriaStoreError> {
    let all_active = provider_store::list_active_providers(pool).await?;

    let criteria = match get_criteria(pool, focus_id).await? {
        None => return Ok(all_active),
        Some(c) => c,
    };

    Ok(all_active
        .into_iter()
        .filter(|p| {
            if criteria.deny_provider_ids.iter().any(|id| id == &p.id) {
                return false;
            }
            if criteria.allow_provider_ids.iter().any(|id| id == &p.id) {
                return true;
            }
            (!criteria.require_is_local || p.is_local)
                && (!criteria.require_is_anonymous || p.is_anonymous)
                && (!criteria.require_not_trains_on_data || !p.trains_on_data_by_default)
        })
        .collect())
}

/// Applies a Focus/step's ExternalAccess ceiling to a candidate provider
/// list (spec Part 6b/6f, items.id=692, decisions.id=870):
///
/// - LocalOnly: no external execution, so no provider qualifies (empty).
/// - AnonymousRequired: hard filter, only `is_anonymous` providers.
/// - AnonymousPreferred: every provider qualifies; `is_anonymous` ones are
///   ranked first (stable -- relative order within each group is kept).
/// - Unrestricted: every provider, order untouched.
///
/// Pure and call-time: ranking is a runtime routing decision, not stored
/// state (spec Part 6f). Ranking only orders the list -- it never selects a
/// provider on the user's behalf or overrides user_provider_preference.
pub fn providers_permitted_by_ceiling(
    mut providers: Vec<Provider>,
    access: ExternalAccess,
) -> Vec<Provider> {
    match access {
        ExternalAccess::LocalOnly => Vec::new(),
        ExternalAccess::AnonymousRequired => {
            providers.retain(|p| p.is_anonymous);
            providers
        }
        ExternalAccess::AnonymousPreferred => {
            // sort_by_key is stable; `false` sorts before `true`.
            providers.sort_by_key(|p| !p.is_anonymous);
            providers
        }
        ExternalAccess::Unrestricted => providers,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Sets QR_DATA_ROOT to a fresh tempdir and migrates shared.db for real
    /// -- so tests below exercise the actual public API against a real
    /// on-disk shared.db, including the real groq/mistral/duckai/claude/
    /// chatgpt/gemini providers rows shared_014.sql/shared_013.sql seed.
    /// Matches user_provider_preference_store.rs's own setup_real_db
    /// pattern (ENV_MUTEX serialization).
    async fn setup_real_db() -> (tempfile::TempDir, sqlx::SqlitePool) {
        let tempdir = tempfile::tempdir().expect("failed to create tempdir");
        std::env::set_var("QR_DATA_ROOT", tempdir.path());
        crate::persistence::migrations::migrate_shared_db()
            .await
            .expect("migrate_shared_db must succeed");
        let pool =
            sqlx::SqlitePool::connect_with(crate::providers::utils::connect_options_unencrypted(
                &crate::providers::utils::db_path_shared(),
            ))
            .await
            .expect("shared.db pool must connect");
        (tempdir, pool)
    }

    #[tokio::test]
    async fn populate_from_policy_local_only_sets_single_flag() {
        let _lock = crate::test_support::ENV_MUTEX.lock().await;
        let saved_root = std::env::var("QR_DATA_ROOT").ok();
        let (_tempdir, pool) = setup_real_db().await;

        let outcome = async {
            let c = populate_from_policy(&pool, "f1", NamedPolicy::LocalOnly).await?;
            assert!(c.require_is_local);
            assert!(!c.require_is_anonymous);
            assert!(!c.require_not_trains_on_data);
            assert_eq!(c.seeded_from_policy, Some(NamedPolicy::LocalOnly));
            Ok::<(), FocusProviderCriteriaStoreError>(())
        }
        .await;

        if let Some(v) = saved_root {
            std::env::set_var("QR_DATA_ROOT", v);
        } else {
            std::env::remove_var("QR_DATA_ROOT");
        }
        outcome.expect("local_only assertions must pass");
    }

    /// Judgment call 2 (revised): local_and_anonymous must set BOTH flags,
    /// not a single-flag shortcut -- so a future non-anonymous local
    /// provider correctly fails this policy while still passing local_only.
    #[tokio::test]
    async fn local_and_anonymous_requires_both_flags() {
        let _lock = crate::test_support::ENV_MUTEX.lock().await;
        let saved_root = std::env::var("QR_DATA_ROOT").ok();
        let (_tempdir, pool) = setup_real_db().await;

        let outcome = async {
            let c = populate_from_policy(&pool, "f1", NamedPolicy::LocalAndAnonymous).await?;
            assert!(
                c.require_is_local,
                "local_and_anonymous must require is_local"
            );
            assert!(
                c.require_is_anonymous,
                "local_and_anonymous must require is_anonymous"
            );
            assert!(!c.require_not_trains_on_data);
            Ok::<(), FocusProviderCriteriaStoreError>(())
        }
        .await;

        if let Some(v) = saved_root {
            std::env::set_var("QR_DATA_ROOT", v);
        } else {
            std::env::remove_var("QR_DATA_ROOT");
        }
        outcome.expect("local_and_anonymous assertions must pass");
    }

    #[tokio::test]
    async fn reset_to_policy_reapplies_and_errors_without_one() {
        let _lock = crate::test_support::ENV_MUTEX.lock().await;
        let saved_root = std::env::var("QR_DATA_ROOT").ok();
        let (_tempdir, pool) = setup_real_db().await;

        let outcome = async {
            populate_from_policy(&pool, "f1", NamedPolicy::LocalOnly).await?;

            // Simulate a tampered/edited flag set that still claims
            // seeded_from_policy='local_only' (bypassing set_criteria, which
            // would clear seeded_from_policy itself -- this isolates
            // reset_to_policy's own "re-read seeded_from_policy and
            // re-apply its flags" behavior from set_criteria's separate
            // clearing behavior, tested on its own below).
            sqlx::query(
                "UPDATE focus_provider_criteria SET require_is_local = 0 WHERE focus_id = 'f1'",
            )
            .execute(&mut *pool.acquire().await?)
            .await?;
            let tampered = get_criteria(&pool, "f1").await?.unwrap();
            assert!(
                !tampered.require_is_local,
                "tampering must have taken effect"
            );

            let reset = reset_to_policy(&pool, "f1").await?;
            assert!(
                reset.require_is_local,
                "reset_to_policy must re-apply local_only's require_is_local"
            );
            assert!(!reset.require_not_trains_on_data);
            assert_eq!(reset.seeded_from_policy, Some(NamedPolicy::LocalOnly));

            set_criteria(&pool, "f2", true, false, false, &[], &[]).await?;
            let err = reset_to_policy(&pool, "f2")
                .await
                .expect_err("a row never seeded from a policy has nothing to reset to");
            assert!(matches!(
                err,
                FocusProviderCriteriaStoreError::Validation(_)
            ));

            Ok::<(), FocusProviderCriteriaStoreError>(())
        }
        .await;

        if let Some(v) = saved_root {
            std::env::set_var("QR_DATA_ROOT", v);
        } else {
            std::env::remove_var("QR_DATA_ROOT");
        }
        outcome.expect("reset_to_policy assertions must pass");
    }

    #[tokio::test]
    async fn set_criteria_clears_seeded_from_policy() {
        let _lock = crate::test_support::ENV_MUTEX.lock().await;
        let saved_root = std::env::var("QR_DATA_ROOT").ok();
        let (_tempdir, pool) = setup_real_db().await;

        let outcome = async {
            populate_from_policy(&pool, "f1", NamedPolicy::Unrestricted).await?;
            let edited = set_criteria(
                &pool,
                "f1",
                true,
                false,
                false,
                &["claude".to_owned()],
                &["duckai".to_owned()],
            )
            .await?;
            assert_eq!(
                edited.seeded_from_policy, None,
                "a direct edit must clear seeded_from_policy"
            );
            assert_eq!(edited.allow_provider_ids, vec!["claude".to_owned()]);
            assert_eq!(edited.deny_provider_ids, vec!["duckai".to_owned()]);
            Ok::<(), FocusProviderCriteriaStoreError>(())
        }
        .await;

        if let Some(v) = saved_root {
            std::env::set_var("QR_DATA_ROOT", v);
        } else {
            std::env::remove_var("QR_DATA_ROOT");
        }
        outcome.expect("set_criteria assertions must pass");
    }

    #[tokio::test]
    async fn eligible_providers_for_focus_no_row_returns_all_active() {
        let _lock = crate::test_support::ENV_MUTEX.lock().await;
        let saved_root = std::env::var("QR_DATA_ROOT").ok();
        let (_tempdir, pool) = setup_real_db().await;

        let outcome = async {
            let eligible = eligible_providers_for_focus(&pool, "no-such-focus").await?;
            let ids: Vec<&str> = eligible.iter().map(|p| p.id.as_str()).collect();
            assert!(ids.contains(&"duckai"));
            assert!(ids.contains(&"claude"));
            assert!(ids.contains(&"groq"));
            assert!(ids.contains(&"mistral"));
            Ok::<(), FocusProviderCriteriaStoreError>(())
        }
        .await;

        if let Some(v) = saved_root {
            std::env::set_var("QR_DATA_ROOT", v);
        } else {
            std::env::remove_var("QR_DATA_ROOT");
        }
        outcome.expect("no-row assertions must pass");
    }

    /// Until items.id=436 (shared_024.sql), no qr_local (is_local=1)
    /// provider rows existed at all, so local_only correctly matched none
    /// -- the spec itself called qr_local metadata "genuinely greenfield"
    /// at the time. shared_024.sql seeds the 3 curated Ollama local_model
    /// rows as is_local=1, so local_only now correctly matches exactly
    /// those three (note: this table's eligibility check is unaffected by
    /// `installed`/`qr_disabled_by_user` -- a curated-but-not-yet-
    /// downloaded model is still `is_local=1` and still "eligible" by this
    /// table's own definition; whether it's actually usable right now is a
    /// different, not-yet-built concern, out of scope here).
    #[tokio::test]
    async fn eligible_providers_for_focus_local_only_matches_ollama_rows() {
        let _lock = crate::test_support::ENV_MUTEX.lock().await;
        let saved_root = std::env::var("QR_DATA_ROOT").ok();
        let (_tempdir, pool) = setup_real_db().await;

        let outcome = async {
            populate_from_policy(&pool, "f1", NamedPolicy::LocalOnly).await?;
            let eligible = eligible_providers_for_focus(&pool, "f1").await?;
            let ids: Vec<&str> = eligible.iter().map(|p| p.id.as_str()).collect();
            assert_eq!(
                ids,
                vec![
                    "ollama:llama3.1:8b",
                    "ollama:llama3.2:3b",
                    "ollama:qwen2.5:7b"
                ],
                "local_only must match exactly the 3 curated is_local=1 Ollama rows"
            );
            Ok::<(), FocusProviderCriteriaStoreError>(())
        }
        .await;

        if let Some(v) = saved_root {
            std::env::set_var("QR_DATA_ROOT", v);
        } else {
            std::env::remove_var("QR_DATA_ROOT");
        }
        outcome.expect("local_only assertions must pass");
    }

    /// Precedence test: deny-list excludes unconditionally -> allow-list
    /// includes unconditionally -> remainder filtered by require_* flags.
    #[tokio::test]
    async fn eligible_providers_for_focus_deny_beats_allow_beats_require() {
        let _lock = crate::test_support::ENV_MUTEX.lock().await;
        let saved_root = std::env::var("QR_DATA_ROOT").ok();
        let (_tempdir, pool) = setup_real_db().await;

        let outcome = async {
            // require_not_trains_on_data=1 alone matches duckai, groq,
            // groqchat, and (items.id=436, shared_024.sql) the 3 curated
            // Ollama local_model rows -- all seeded with
            // trains_on_data_by_default=0 (Tier 1, the most private
            // option). (items.id=440 Part A curated groq as not training on
            // data by default -- no longer just duckai, as it was when this
            // test was first written against groq's pre-curation placeholder
            // seed. shared_018.sql -- items.id=347/486-adjacent provider
            // catalog work -- added groqchat, Groq's own cloud_frontier web-chat row,
            // reusing the same items.id=440 no-training finding since it's
            // the same real-world entity/policy, not a new curation).
            // deny=[duckai] excludes it anyway despite passing the
            // requirement; allow=[claude] carves claude in despite it
            // failing the requirement (trains_on_data=1); groq, groqchat,
            // and the 3 Ollama rows all pass the requirement on their own
            // merits and need neither list.
            set_criteria(
                &pool,
                "f1",
                false,
                false,
                true,
                &["claude".to_owned()],
                &["duckai".to_owned()],
            )
            .await?;

            let eligible = eligible_providers_for_focus(&pool, "f1").await?;
            let ids: Vec<&str> = eligible.iter().map(|p| p.id.as_str()).collect();
            assert_eq!(
                ids,
                vec![
                    "groq",
                    "claude",
                    "groqchat",
                    "ollama:llama3.1:8b",
                    "ollama:llama3.2:3b",
                    "ollama:qwen2.5:7b"
                ],
                "deny must beat a passing require match (duckai), allow must beat a failing \
                 require match (claude), and groq/groqchat/the 3 Ollama rows must each pass \
                 the requirement on their own merits -- everything else fails the requirement"
            );
            Ok::<(), FocusProviderCriteriaStoreError>(())
        }
        .await;

        if let Some(v) = saved_root {
            std::env::set_var("QR_DATA_ROOT", v);
        } else {
            std::env::remove_var("QR_DATA_ROOT");
        }
        outcome.expect("deny-beats-allow-beats-require assertions must pass");
    }

    // -- providers_permitted_by_ceiling (items.id=692, decisions.id=870) ------

    fn ids(providers: &[Provider]) -> Vec<&str> {
        providers.iter().map(|p| p.id.as_str()).collect()
    }

    /// Runs `f` against the real seeded active-provider pool (groq, mistral,
    /// duckai, claude, ...), restoring QR_DATA_ROOT afterwards.
    async fn with_seeded_providers<F>(f: F)
    where
        F: FnOnce(Vec<Provider>, Vec<Provider>),
    {
        let _lock = crate::test_support::ENV_MUTEX.lock().await;
        let saved_root = std::env::var("QR_DATA_ROOT").ok();
        let (_tempdir, pool) = setup_real_db().await;

        let all = provider_store::list_active_providers(&pool)
            .await
            .expect("list_active_providers must succeed");
        let hosted = provider_store::list_providers_by_type(&pool, "cloud_inference_api")
            .await
            .expect("list_providers_by_type must succeed");

        if let Some(v) = saved_root {
            std::env::set_var("QR_DATA_ROOT", v);
        } else {
            std::env::remove_var("QR_DATA_ROOT");
        }
        f(all, hosted);
    }

    #[tokio::test]
    async fn anonymous_required_excludes_groq_and_mistral() {
        with_seeded_providers(|all, hosted| {
            assert_eq!(ids(&hosted), vec!["groq", "mistral"]);
            assert!(hosted.iter().all(|p| !p.is_anonymous));

            let eligible =
                providers_permitted_by_ceiling(hosted, ExternalAccess::AnonymousRequired);
            assert!(
                eligible.is_empty(),
                "no API-routable provider is anonymous in R1"
            );

            // Hard filter over the whole pool: only is_anonymous rows survive.
            let eligible_all =
                providers_permitted_by_ceiling(all, ExternalAccess::AnonymousRequired);
            assert!(!eligible_all.is_empty());
            assert!(eligible_all.iter().all(|p| p.is_anonymous));
            assert!(!ids(&eligible_all).contains(&"groq"));
            assert!(!ids(&eligible_all).contains(&"mistral"));
        })
        .await;
    }

    #[tokio::test]
    async fn anonymous_preferred_allows_everyone_and_ranks_anonymous_first() {
        with_seeded_providers(|all, hosted| {
            let original: Vec<String> = all.iter().map(|p| p.id.clone()).collect();
            let ranked = providers_permitted_by_ceiling(all, ExternalAccess::AnonymousPreferred);

            // Allows every provider, none dropped.
            let mut got: Vec<String> = ranked.iter().map(|p| p.id.clone()).collect();
            let mut want = original.clone();
            got.sort();
            want.sort();
            assert_eq!(got, want);

            // Ranked: no non-anonymous row precedes an anonymous one.
            let first_non_anon = ranked.iter().position(|p| !p.is_anonymous);
            let last_anon = ranked.iter().rposition(|p| p.is_anonymous);
            if let (Some(n), Some(a)) = (first_non_anon, last_anon) {
                assert!(a < n, "anonymous providers must all rank before the rest");
            }
            assert!(
                ranked.first().is_some_and(|p| p.is_anonymous),
                "the seeded catalog contains anonymous providers (duckai)"
            );

            // Stable within each group: relative order of the hosted pair is
            // untouched, and hosted providers are still allowed.
            let ranked_hosted =
                providers_permitted_by_ceiling(hosted, ExternalAccess::AnonymousPreferred);
            assert_eq!(ids(&ranked_hosted), vec!["groq", "mistral"]);
        })
        .await;
    }

    #[tokio::test]
    async fn unrestricted_allows_everyone_in_original_order() {
        with_seeded_providers(|all, _hosted| {
            let original: Vec<String> = all.iter().map(|p| p.id.clone()).collect();
            let eligible = providers_permitted_by_ceiling(all, ExternalAccess::Unrestricted);
            let got: Vec<String> = eligible.iter().map(|p| p.id.clone()).collect();
            assert_eq!(got, original);
        })
        .await;
    }

    #[tokio::test]
    async fn local_only_permits_no_external_provider() {
        with_seeded_providers(|all, _hosted| {
            assert!(providers_permitted_by_ceiling(all, ExternalAccess::LocalOnly).is_empty());
        })
        .await;
    }
}
