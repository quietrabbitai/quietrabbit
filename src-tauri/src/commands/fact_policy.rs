// src-tauri/src/commands/fact_policy.rs
//
// items.id=930 stage 4 (decisions.id=953 ruling 6): the user's own commands for the
// DISCLOSURE POLICY of the closed catalogue of personal facts (location_city,
// job_title, employer, industry): what each may do at the two external tiers when a
// step that goes outside the device declares it. No UI is built by this item.
//
//   get_fact_policies(persona_id)
//       one entry per catalogued field: its label and sensitivity (the catalogue's),
//       and for tier 2 and tier 3 the user's stored choice (None when none), the
//       catalogue default, the EFFECTIVE policy the resolver gives today and whether
//       the user may choose pass there. Read only.
//   set_fact_policy(persona_id, field_name, tier, policy)
//       one (field, tier) choice. tier is the NUMBER 2 or 3: any other number is
//       refused (invalid_tier), never mapped to omit. policy is a stored word: pass,
//       summarize, omit or ask. Validated by fact_policy_store::set_fact_policy
//       against the catalogue and the hard floors (a pass on a medical or financial
//       field, a pass at a tier the field does not allow, not_permitted and any
//       other word are refused with nothing written). Returns the field's new entry.
//
// Persona-level, not Focus-level, and not collection-consent-gated: a user's own
// choice always works. NOT wired into Gate 1 or any run: the bridge is items.id=931.
//
// EVERY command: takes writer_gate::command first (a key rotation waits for it),
// derives the user and the data key from the unlocked session through
// persona::session_user_and_key (no user_id argument exists to spoof), checks the
// Persona id is a UUID and belongs to that user (require_persona, private and written
// so items.id=638 can lift it) and does its work in a *_for_session body with no
// Tauri state, tested directly.
//
// ERROR CODES (ERROR_CODES, each with a placeholder key fact_policy.error.<code> in
// the frontend's en.json, a test requires it): persona_not_found, unknown_field,
// invalid_tier, invalid_policy, pass_not_allowed, tier3_not_selectable,
// catalogue_unavailable, fact_policy_unavailable. The last is any store failure and
// carries no detail. The wording is placeholder (Chat-BRAND). The user-facing words
// share, describe and withhold are placeholder keys fact_policy.policy.<word> for the
// stored words pass, summarize and omit (and ask, reserved until items.id=924).

use serde::Serialize;
use specta::Type;
use tauri::State;

use crate::auth::registry::KeyRegistry;
use crate::commands::persona::session_user_and_key;
use crate::conductor::personal_fact_fields::{
    self, effective_policy, ChoiceError, Origin, PolicyInput, Tier,
};
use crate::persistence::fact_policy_store::{self, FactPolicyError, UserPolicies};
use crate::persistence::persona_store;
use crate::persistence::personal_store::open_personal_db;

/// Every error string this module returns.
pub const ERROR_CODES: [&str; 8] = [
    "persona_not_found",
    "unknown_field",
    "invalid_tier",
    "invalid_policy",
    "pass_not_allowed",
    "tier3_not_selectable",
    "catalogue_unavailable",
    "fact_policy_unavailable",
];

/// One external tier of one field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Type)]
pub struct FactPolicyTier {
    /// The tier number: 2 or 3.
    pub tier: i32,
    /// The user's stored choice (pass, summarize, omit or ask), or None when the user
    /// has made none or the stored value is not one a user may choose.
    pub choice: Option<String>,
    /// The catalogue default.
    pub default_policy: String,
    /// What the resolver gives today: the choice if valid, else the default, after the
    /// floors.
    pub effective: String,
    /// Whether the user may choose pass here.
    pub can_share: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Type)]
pub struct FactPolicyInfo {
    pub field_name: String,
    pub label: String,
    /// The catalogue's sensitivity (general, personal, medical or financial).
    pub sensitivity: String,
    pub tier2: FactPolicyTier,
    pub tier3: FactPolicyTier,
}

fn unavailable(what: &str, e: impl std::fmt::Display) -> String {
    // The text names a step and a store problem, never a value.
    log::warn!("fact policy: {what}: {e}");
    "fact_policy_unavailable".to_owned()
}

/// The Persona id must be a canonical UUID AND belong to the signed-in user. The two
/// failures answer alike (persona_not_found). Private on purpose, so items.id=638 can
/// lift it into one shared check with the other copies.
async fn require_persona(
    pool: &sqlx::SqlitePool,
    user_id: &str,
    persona_id: &str,
) -> Result<(), String> {
    let canonical = uuid::Uuid::parse_str(persona_id)
        .map(|u| u.hyphenated().to_string() == persona_id)
        .unwrap_or(false);
    if !canonical {
        return Err("persona_not_found".to_owned());
    }
    match persona_store::is_user_in_persona(pool, user_id, persona_id).await {
        Ok(true) => Ok(()),
        Ok(false) => Err("persona_not_found".to_owned()),
        Err(e) => Err(unavailable("persona check", e)),
    }
}

fn tier_info(
    catalogue: &personal_fact_fields::Catalogue,
    def: &personal_fact_fields::FieldDef,
    user: &UserPolicies,
    tier: Tier,
) -> FactPolicyTier {
    let number = if tier == Tier::Two { 2 } else { 3 };
    let raw = user.lookup(&def.name, tier);
    let effective = effective_policy(
        Ok(catalogue),
        &PolicyInput {
            field_name: &def.name,
            stored_sensitivity: &def.sensitivity,
            origin: Origin::ThisPersona,
            tier,
            user_policy: raw,
        },
    );
    let default_policy = match tier {
        Tier::Two => def.tier2_default.clone(),
        Tier::Three => def.tier3_default.clone(),
    };
    FactPolicyTier {
        tier: number,
        // Only a word a user may choose is shown as a choice.
        choice: raw
            .filter(|r| personal_fact_fields::Policy::parse_user(r).is_some())
            .map(str::to_owned),
        default_policy,
        effective: effective.as_str().to_owned(),
        can_share: personal_fact_fields::check_user_choice(catalogue, &def.name, tier, "pass")
            .is_ok(),
    }
}

fn info_for(
    catalogue: &personal_fact_fields::Catalogue,
    def: &personal_fact_fields::FieldDef,
    user: &UserPolicies,
) -> FactPolicyInfo {
    FactPolicyInfo {
        field_name: def.name.clone(),
        label: def.label.clone(),
        sensitivity: def.sensitivity.clone(),
        tier2: tier_info(catalogue, def, user, Tier::Two),
        tier3: tier_info(catalogue, def, user, Tier::Three),
    }
}

#[tauri::command]
#[specta::specta]
pub async fn get_fact_policies(
    persona_id: String,
    key_registry: State<'_, KeyRegistry>,
    pool: State<'_, sqlx::SqlitePool>,
) -> Result<Vec<FactPolicyInfo>, String> {
    let _lease = crate::auth::writer_gate::command(&key_registry).await?;
    let (user_id, key_hex) = session_user_and_key(&key_registry).await?;
    get_fact_policies_for_session(&pool, &user_id, &key_hex, &persona_id).await
}

#[tauri::command]
#[specta::specta]
pub async fn set_fact_policy(
    persona_id: String,
    field_name: String,
    tier: i32,
    policy: String,
    key_registry: State<'_, KeyRegistry>,
    pool: State<'_, sqlx::SqlitePool>,
) -> Result<FactPolicyInfo, String> {
    let _lease = crate::auth::writer_gate::command(&key_registry).await?;
    let (user_id, key_hex) = session_user_and_key(&key_registry).await?;
    set_fact_policy_for_session(
        &pool,
        &user_id,
        &key_hex,
        &persona_id,
        &field_name,
        tier,
        &policy,
    )
    .await
}

/// The code a store error becomes at the command boundary.
fn code_for(e: FactPolicyError) -> String {
    match e {
        FactPolicyError::Choice(ChoiceError::UnknownField) => "unknown_field".to_owned(),
        FactPolicyError::Choice(ChoiceError::UnknownPolicy) => "invalid_policy".to_owned(),
        FactPolicyError::Choice(ChoiceError::FloorForbidsPass) => "pass_not_allowed".to_owned(),
        FactPolicyError::Choice(ChoiceError::Tier3NotSelectable) => {
            "tier3_not_selectable".to_owned()
        }
        FactPolicyError::CatalogueUnreadable => "catalogue_unavailable".to_owned(),
        FactPolicyError::BadSetBy | FactPolicyError::Database(_) => {
            unavailable("set_fact_policy", e)
        }
    }
}

pub(crate) async fn get_fact_policies_for_session(
    pool: &sqlx::SqlitePool,
    user_id: &str,
    key_hex: &str,
    persona_id: &str,
) -> Result<Vec<FactPolicyInfo>, String> {
    require_persona(pool, user_id, persona_id).await?;
    let catalogue = personal_fact_fields::registry().map_err(|e| {
        log::warn!("fact policy: catalogue: {e}");
        "catalogue_unavailable".to_owned()
    })?;
    let mut conn = open_personal_db(user_id, persona_id, key_hex)
        .await
        .map_err(|e| unavailable("open personal.db", e))?;
    // A table that cannot be read shows every field as omit, the same answer a run
    // would get (fail closed).
    let user = UserPolicies::load(&mut conn).await;
    Ok(catalogue
        .fields
        .iter()
        .map(|def| info_for(catalogue, def, &user))
        .collect())
}

pub(crate) async fn set_fact_policy_for_session(
    pool: &sqlx::SqlitePool,
    user_id: &str,
    key_hex: &str,
    persona_id: &str,
    field_name: &str,
    tier: i32,
    policy: &str,
) -> Result<FactPolicyInfo, String> {
    require_persona(pool, user_id, persona_id).await?;
    // An unknown tier number is a refusal, never mapped to omit (the bridge's own
    // fail-closed mapping is for stored data, not for a caller's request).
    let tier = Tier::from_number(i64::from(tier)).ok_or_else(|| "invalid_tier".to_owned())?;
    let catalogue = personal_fact_fields::registry().map_err(|e| {
        log::warn!("fact policy: catalogue: {e}");
        "catalogue_unavailable".to_owned()
    })?;
    let mut conn = open_personal_db(user_id, persona_id, key_hex)
        .await
        .map_err(|e| unavailable("open personal.db", e))?;
    fact_policy_store::set_fact_policy(
        &mut conn,
        field_name,
        tier,
        policy,
        fact_policy_store::SET_BY_USER,
    )
    .await
    .map_err(code_for)?;
    let user = UserPolicies::load(&mut conn).await;
    let def = catalogue
        .field(field_name)
        .ok_or_else(|| "unknown_field".to_owned())?;
    Ok(info_for(catalogue, def, &user))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::registry::key_hex;
    use crate::test_support::ENV_MUTEX;
    use sqlx::Row;

    const MASTER_KEY: [u8; crate::auth::kdf::MASTER_KEY_LEN] =
        [0xABu8; crate::auth::kdf::MASTER_KEY_LEN];

    struct Env {
        _tempdir: tempfile::TempDir,
        _lock: tokio::sync::MutexGuard<'static, ()>,
        saved_root: Option<String>,
        pool: sqlx::SqlitePool,
        user: String,
        persona: String,
        key: String,
    }

    impl Drop for Env {
        fn drop(&mut self) {
            match &self.saved_root {
                Some(v) => std::env::set_var("QR_DATA_ROOT", v),
                None => std::env::remove_var("QR_DATA_ROOT"),
            }
        }
    }

    async fn add_user(pool: &sqlx::SqlitePool, name: &str) -> String {
        let id = uuid::Uuid::new_v4().to_string();
        crate::auth::user_store::create_user(
            pool,
            &id,
            name,
            "user",
            false,
            &[0u8; crate::auth::kdf::SALT_LEN],
            crate::auth::kdf::DEFAULT_ARGON2_MEMORY_KIB,
            crate::auth::kdf::DEFAULT_ARGON2_ITERATIONS,
            crate::auth::kdf::DEFAULT_ARGON2_PARALLELISM,
            &[0u8; 32],
        )
        .await
        .expect("create_user");
        id
    }

    async fn env() -> Env {
        let lock = ENV_MUTEX.lock().await;
        let saved_root = std::env::var("QR_DATA_ROOT").ok();
        let tempdir = tempfile::tempdir().expect("tempdir");
        std::env::set_var("QR_DATA_ROOT", tempdir.path());
        crate::persistence::migrations::migrate_shared_db()
            .await
            .expect("shared.db");
        let pool =
            sqlx::SqlitePool::connect_with(crate::providers::utils::connect_options_unencrypted(
                &crate::providers::utils::db_path_shared(),
            ))
            .await
            .expect("pool");
        let key = key_hex(&MASTER_KEY);
        let user = add_user(&pool, "Policy User").await;
        let persona = uuid::Uuid::new_v4().to_string();
        persona_store::create_persona(&pool, &persona, "P", "personal", &user, &key, None)
            .await
            .expect("create_persona");
        Env {
            _tempdir: tempdir,
            _lock: lock,
            saved_root,
            pool,
            user,
            persona,
            key,
        }
    }

    async fn get(e: &Env) -> Result<Vec<FactPolicyInfo>, String> {
        get_fact_policies_for_session(&e.pool, &e.user, &e.key, &e.persona).await
    }

    async fn set(e: &Env, field: &str, tier: i32, policy: &str) -> Result<FactPolicyInfo, String> {
        set_fact_policy_for_session(&e.pool, &e.user, &e.key, &e.persona, field, tier, policy).await
    }

    async fn row_count(e: &Env) -> i64 {
        let mut conn = open_personal_db(&e.user, &e.persona, &e.key).await.unwrap();
        sqlx::query("SELECT COUNT(*) FROM fact_disclosure_policy")
            .fetch_one(&mut conn)
            .await
            .unwrap()
            .get(0)
    }

    #[tokio::test]
    async fn a_fresh_persona_lists_the_four_fields_at_omit_with_no_choice() {
        let e = env().await;
        let list = get(&e).await.unwrap();
        let names: Vec<&str> = list.iter().map(|f| f.field_name.as_str()).collect();
        assert_eq!(
            names,
            ["location_city", "job_title", "employer", "industry"]
        );
        for f in &list {
            assert_eq!(f.sensitivity, "personal");
            assert!(!f.label.is_empty());
            for (t, n) in [(&f.tier2, 2), (&f.tier3, 3)] {
                assert_eq!(t.tier, n);
                assert_eq!(t.choice, None);
                assert_eq!(t.default_policy, "omit");
                assert_eq!(t.effective, "omit");
            }
            assert!(
                f.tier2.can_share,
                "tier 2 pass is allowed for a personal field"
            );
            assert!(!f.tier3.can_share, "no v1 field is tier3_selectable");
        }
    }

    #[tokio::test]
    async fn set_stores_the_choice_returns_the_entry_and_get_shows_it() {
        let e = env().await;
        let r = set(&e, "job_title", 2, "pass").await.unwrap();
        assert_eq!(r.field_name, "job_title");
        assert_eq!(r.tier2.choice.as_deref(), Some("pass"));
        assert_eq!(r.tier2.effective, "pass");
        assert_eq!(r.tier3.choice, None);
        assert_eq!(r.tier3.effective, "omit");
        let r = set(&e, "job_title", 3, "summarize").await.unwrap();
        assert_eq!(r.tier3.effective, "summarize");
        assert_eq!(
            r.tier2.choice.as_deref(),
            Some("pass"),
            "the other tier is kept"
        );
        let r = set(&e, "job_title", 2, "ask").await.unwrap();
        assert_eq!(r.tier2.choice.as_deref(), Some("ask"));
        let listed = get(&e).await.unwrap();
        assert_eq!(
            listed.iter().find(|f| f.field_name == "job_title"),
            Some(&r)
        );
        assert_eq!(
            listed
                .iter()
                .find(|f| f.field_name == "employer")
                .unwrap()
                .tier2
                .choice,
            None
        );
    }

    #[tokio::test]
    async fn every_refusal_has_its_code_and_writes_nothing() {
        let e = env().await;
        let cases = [
            ("income_range", 2, "omit", "unknown_field"),
            ("job_title", 1, "omit", "invalid_tier"),
            ("job_title", 4, "omit", "invalid_tier"),
            ("job_title", 0, "omit", "invalid_tier"),
            ("job_title", -2, "omit", "invalid_tier"),
            ("job_title", i32::MAX, "omit", "invalid_tier"),
            ("job_title", 2, "not_permitted", "invalid_policy"),
            ("job_title", 2, "share", "invalid_policy"),
            ("job_title", 2, "", "invalid_policy"),
            ("job_title", 3, "pass", "tier3_not_selectable"),
        ];
        for (field, tier, policy, code) in cases {
            let err = set(&e, field, tier, policy).await.unwrap_err();
            assert_eq!(err, code, "{field} {tier} {policy}");
            assert!(ERROR_CODES.contains(&err.as_str()));
        }
        assert_eq!(row_count(&e).await, 0);
    }

    #[test]
    fn the_store_errors_map_to_the_documented_codes() {
        // The shipped catalogue has no medical or financial field, so the floor error is
        // produced from a modified copy and mapped by the same function.
        let mut c = personal_fact_fields::registry().unwrap().clone();
        c.fields[1].sensitivity = "financial".to_owned();
        let floor = personal_fact_fields::check_user_choice(&c, "job_title", Tier::Two, "pass")
            .unwrap_err();
        let all = [
            (FactPolicyError::Choice(floor), "pass_not_allowed"),
            (
                FactPolicyError::Choice(ChoiceError::UnknownField),
                "unknown_field",
            ),
            (
                FactPolicyError::Choice(ChoiceError::UnknownPolicy),
                "invalid_policy",
            ),
            (
                FactPolicyError::Choice(ChoiceError::Tier3NotSelectable),
                "tier3_not_selectable",
            ),
            (
                FactPolicyError::CatalogueUnreadable,
                "catalogue_unavailable",
            ),
            (FactPolicyError::BadSetBy, "fact_policy_unavailable"),
            (
                FactPolicyError::Database(sqlx::Error::RowNotFound),
                "fact_policy_unavailable",
            ),
        ];
        for (err, want) in all {
            let got = code_for(err);
            assert_eq!(got, want);
            assert!(ERROR_CODES.contains(&got.as_str()));
        }
    }

    #[tokio::test]
    async fn a_foreign_or_malformed_persona_is_persona_not_found() {
        let e = env().await;
        let other_user = add_user(&e.pool, "Other").await;
        for persona in [
            "not-a-uuid".to_owned(),
            uuid::Uuid::new_v4().to_string(),
            e.persona.to_uppercase(),
        ] {
            assert_eq!(
                get_fact_policies_for_session(&e.pool, &e.user, &e.key, &persona)
                    .await
                    .unwrap_err(),
                "persona_not_found"
            );
        }
        assert_eq!(
            get_fact_policies_for_session(&e.pool, &other_user, &e.key, &e.persona)
                .await
                .unwrap_err(),
            "persona_not_found"
        );
        assert_eq!(
            set_fact_policy_for_session(
                &e.pool,
                &other_user,
                &e.key,
                &e.persona,
                "job_title",
                2,
                "omit"
            )
            .await
            .unwrap_err(),
            "persona_not_found"
        );
        assert_eq!(row_count(&e).await, 0);
    }

    #[tokio::test]
    async fn a_garbage_row_shows_no_choice_and_an_effective_omit() {
        let e = env().await;
        let mut conn = open_personal_db(&e.user, &e.persona, &e.key).await.unwrap();
        sqlx::query(
            "INSERT INTO fact_disclosure_policy (field_name, tier2, tier3, set_at, set_by)
             VALUES ('job_title', 'not_permitted', 'whatever', 't', 'user')",
        )
        .execute(&mut conn)
        .await
        .unwrap();
        drop(conn);
        let f = get(&e)
            .await
            .unwrap()
            .into_iter()
            .find(|f| f.field_name == "job_title")
            .unwrap();
        for t in [&f.tier2, &f.tier3] {
            assert_eq!(t.choice, None);
            assert_eq!(t.effective, "omit");
        }
    }

    /// Every error code has a placeholder key in the frontend's en.json (Chat-BRAND
    /// owns the wording), and so does every stored policy word.
    #[test]
    fn every_error_code_and_policy_word_has_a_placeholder_key_in_en_json() {
        let json: serde_json::Value =
            serde_json::from_str(include_str!("../../../frontend/src/i18n/en.json")).unwrap();
        for code in ERROR_CODES {
            let text = json["fact_policy"]["error"][code]
                .as_str()
                .unwrap_or_else(|| panic!("en.json has no fact_policy.error.{code}"));
            assert!(!text.trim().is_empty(), "{code}");
        }
        let keys: Vec<&String> = json["fact_policy"]["error"]
            .as_object()
            .unwrap()
            .keys()
            .collect();
        assert_eq!(keys.len(), ERROR_CODES.len(), "{keys:?}");
        for word in ["pass", "summarize", "omit", "ask"] {
            let text = json["fact_policy"]["policy"][word]
                .as_str()
                .unwrap_or_else(|| panic!("en.json has no fact_policy.policy.{word}"));
            assert!(!text.trim().is_empty(), "{word}");
        }
        assert_eq!(json["fact_policy"]["policy"].as_object().unwrap().len(), 4);
    }

    /// The commands take the writer gate first, take no user id, and never name a
    /// fact value, the entity_facts abstraction columns or Gate 1.
    #[test]
    fn the_commands_follow_the_consent_command_pattern() {
        let src = include_str!("fact_policy.rs");
        let body = src.split("#[cfg(test)]").next().unwrap();
        assert_eq!(
            body.matches("writer_gate::command(&key_registry)").count(),
            2
        );
        assert_eq!(
            body.matches("session_user_and_key(&key_registry)").count(),
            2
        );
        let code: String = body
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!code.contains("user_id: String"));
        for banned in [
            "abstraction_tier",
            "field_value",
            "entity_facts",
            "gate1",
            "Gate1",
        ] {
            assert!(!code.contains(banned), "{banned}");
        }
    }
}
