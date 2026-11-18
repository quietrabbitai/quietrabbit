// src-tauri/src/persistence/fact_policy_store.rs
//
// items.id=930 stage 3 (decisions.id=953 rulings 2, 6, 10): the user's per-field
// disclosure policy for catalogued personal facts, backed by fact_disclosure_policy
// in the Persona's encrypted personal.db (schema/personal_012.sql).
//
// ONE WRITER. set_fact_policy() is the only function that writes the table, and it
// validates against the catalogue and the hard floors first (conductor::
// personal_fact_fields::check_user_choice): a name that is not catalogued, a word a
// user may not choose (not_permitted is system-only), a pass on a medical or
// financial field and a pass at a tier the field does not allow are all refused
// with nothing written. A source scan pins that nothing else writes the table.
// There is no delete: the way back to the default is to choose omit.
//
// READS return the RAW stored strings and never interpret them: effective_policy()
// is the one place a choice becomes a policy, and it treats a value it cannot parse
// as omit. UserPolicies::load() is the fail-closed read the bridge (items.id=931)
// uses: a table that is missing or unreadable makes every lookup answer omit
// (ruling 10), never "no row".
//
// NOT WIRED into Gate 1 or any run in this item. Nothing here reads the entity_facts
// abstraction columns (a scan pins it), and no value of a fact passes through here:
// the table holds field names and policy words only.
//
// CONNECTION MODEL: functions take the caller's &mut SqliteConnection to an opened
// personal.db (open_personal_db), like every other per-Persona store here.

use sqlx::{Row, SqliteConnection};
use thiserror::Error;

use crate::conductor::personal_fact_fields::{self, ChoiceError, Tier};

/// Who may be recorded as having set a policy. R1 has the user only.
pub const SET_BY_USER: &str = "user";

#[derive(Debug, Error)]
pub enum FactPolicyError {
    #[error("the personal fact catalogue is unreadable")]
    CatalogueUnreadable,
    #[error("{0}")]
    Choice(#[from] ChoiceError),
    #[error("set_by is not an allowed value")]
    BadSetBy,
    #[error("Database error: {0}")]
    Database(#[from] sqlx::Error),
}

/// One stored row, strings exactly as stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FactPolicyRow {
    pub field_name: String,
    pub tier2: Option<String>,
    pub tier3: Option<String>,
    pub set_at: String,
    pub set_by: String,
}

impl FactPolicyRow {
    pub fn for_tier(&self, tier: Tier) -> Option<&str> {
        match tier {
            Tier::Two => self.tier2.as_deref(),
            Tier::Three => self.tier3.as_deref(),
        }
    }
}

/// Validate and store the user's choice for one (field, tier). The other tier of the
/// same field is left as it was. Returns the stored row.
pub async fn set_fact_policy(
    conn: &mut SqliteConnection,
    field_name: &str,
    tier: Tier,
    policy: &str,
    set_by: &str,
) -> Result<FactPolicyRow, FactPolicyError> {
    if set_by != SET_BY_USER {
        return Err(FactPolicyError::BadSetBy);
    }
    let catalogue =
        personal_fact_fields::registry().map_err(|_| FactPolicyError::CatalogueUnreadable)?;
    let chosen = personal_fact_fields::check_user_choice(catalogue, field_name, tier, policy)?;
    let now = crate::providers::utils::now();

    // The column name comes from the closed Tier enum, never from input.
    let sql = match tier {
        Tier::Two => {
            "INSERT INTO fact_disclosure_policy (field_name, tier2, set_at, set_by)
             VALUES (?, ?, ?, ?)
             ON CONFLICT(field_name) DO UPDATE SET
                 tier2 = excluded.tier2, set_at = excluded.set_at, set_by = excluded.set_by"
        }
        Tier::Three => {
            "INSERT INTO fact_disclosure_policy (field_name, tier3, set_at, set_by)
             VALUES (?, ?, ?, ?)
             ON CONFLICT(field_name) DO UPDATE SET
                 tier3 = excluded.tier3, set_at = excluded.set_at, set_by = excluded.set_by"
        }
    };
    sqlx::query(sql)
        .bind(field_name)
        .bind(chosen.as_str())
        .bind(&now)
        .bind(set_by)
        .execute(&mut *conn)
        .await?;

    let row = list_rows(conn)
        .await?
        .into_iter()
        .find(|r| r.field_name == field_name)
        .ok_or(FactPolicyError::Database(sqlx::Error::RowNotFound))?;
    Ok(row)
}

/// Every stored row, in field-name order. Raw strings.
pub async fn list_rows(conn: &mut SqliteConnection) -> Result<Vec<FactPolicyRow>, FactPolicyError> {
    let rows = sqlx::query(
        "SELECT field_name, tier2, tier3, set_at, set_by FROM fact_disclosure_policy
         ORDER BY field_name",
    )
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows
        .iter()
        .map(|r| FactPolicyRow {
            field_name: r.get("field_name"),
            tier2: r.get("tier2"),
            tier3: r.get("tier3"),
            set_at: r.get("set_at"),
            set_by: r.get("set_by"),
        })
        .collect())
}

/// The user's choices as the resolver reads them, with the fail-closed rule of
/// decisions.id=953 ruling 10: a table that cannot be read answers "omit" for every
/// field and tier, never "no row" (which would fall through to the catalogue default).
#[derive(Debug, Clone)]
pub enum UserPolicies {
    Rows(Vec<FactPolicyRow>),
    Unreadable,
}

impl UserPolicies {
    pub async fn load(conn: &mut SqliteConnection) -> UserPolicies {
        match list_rows(conn).await {
            Ok(rows) => UserPolicies::Rows(rows),
            Err(e) => {
                // The error text names a table or a SQL problem, never a value.
                log::warn!("fact_disclosure_policy unreadable, every field is omit: {e}");
                UserPolicies::Unreadable
            }
        }
    }

    /// The raw user policy string for effective_policy's `user_policy`.
    pub fn lookup(&self, field_name: &str, tier: Tier) -> Option<&str> {
        match self {
            UserPolicies::Unreadable => Some("omit"),
            UserPolicies::Rows(rows) => rows
                .iter()
                .find(|r| r.field_name == field_name)
                .and_then(|r| r.for_tier(tier)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conductor::personal_fact_fields::{effective_policy, Origin, Policy, PolicyInput};

    const KEY: &str = "deadbeef00112233445566778899aabbccddeeff00112233445566778899aabb";

    async fn with_db<F, Fut>(f: F)
    where
        F: FnOnce(SqliteConnection) -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        let _lock = crate::test_support::ENV_MUTEX.lock().await;
        let saved = std::env::var("QR_DATA_ROOT").ok();
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("QR_DATA_ROOT", dir.path());
        let conn =
            crate::persistence::personal_store::open_personal_db("fp-user", "fp-persona", KEY)
                .await
                .unwrap();
        f(conn).await;
        match saved {
            Some(v) => std::env::set_var("QR_DATA_ROOT", v),
            None => std::env::remove_var("QR_DATA_ROOT"),
        }
    }

    fn resolved(user: Option<&str>, tier: Tier) -> Policy {
        let c = personal_fact_fields::registry().unwrap();
        effective_policy(
            Ok(c),
            &PolicyInput {
                field_name: "job_title",
                stored_sensitivity: "personal",
                origin: Origin::ThisPersona,
                tier,
                user_policy: user,
            },
        )
    }

    #[tokio::test]
    async fn a_valid_choice_is_stored_per_tier_and_the_other_tier_is_untouched() {
        with_db(|mut conn| async move {
            assert!(list_rows(&mut conn).await.unwrap().is_empty());
            let r = set_fact_policy(&mut conn, "job_title", Tier::Two, "pass", SET_BY_USER)
                .await
                .unwrap();
            assert_eq!(r.tier2.as_deref(), Some("pass"));
            assert_eq!(r.tier3, None);
            assert_eq!(r.set_by, "user");
            // A second tier for the same field and a replacement of the first.
            set_fact_policy(
                &mut conn,
                "job_title",
                Tier::Three,
                "summarize",
                SET_BY_USER,
            )
            .await
            .unwrap();
            let r = set_fact_policy(&mut conn, "job_title", Tier::Two, "ask", SET_BY_USER)
                .await
                .unwrap();
            assert_eq!(r.tier2.as_deref(), Some("ask"));
            assert_eq!(r.tier3.as_deref(), Some("summarize"));
            assert_eq!(list_rows(&mut conn).await.unwrap().len(), 1);
            // Another field is its own row.
            set_fact_policy(&mut conn, "employer", Tier::Two, "omit", SET_BY_USER)
                .await
                .unwrap();
            let names: Vec<_> = list_rows(&mut conn)
                .await
                .unwrap()
                .into_iter()
                .map(|r| r.field_name)
                .collect();
            assert_eq!(names, ["employer", "job_title"]);
        })
        .await;
    }

    #[tokio::test]
    async fn every_refused_choice_writes_nothing() {
        with_db(|mut conn| async move {
            let cases: [(&str, Tier, &str, &str); 7] = [
                ("income_range", Tier::Two, "omit", "not catalogued"),
                ("sender_name", Tier::Two, "pass", "not in v1"),
                ("job_title", Tier::Two, "not_permitted", "system only"),
                (
                    "job_title",
                    Tier::Two,
                    "share",
                    "user-facing word is not stored",
                ),
                ("job_title", Tier::Two, "", "empty"),
                ("job_title", Tier::Three, "pass", "not tier3_selectable"),
                ("JOB_TITLE", Tier::Two, "omit", "name is exact"),
            ];
            for (field, tier, policy, why) in cases {
                assert!(
                    set_fact_policy(&mut conn, field, tier, policy, SET_BY_USER)
                        .await
                        .is_err(),
                    "{why}"
                );
            }
            assert!(matches!(
                set_fact_policy(&mut conn, "job_title", Tier::Two, "omit", "model").await,
                Err(FactPolicyError::BadSetBy)
            ));
            assert!(list_rows(&mut conn).await.unwrap().is_empty());
        })
        .await;
    }

    #[tokio::test]
    async fn the_floors_refuse_a_pass_on_a_medical_or_financial_catalogue_field() {
        // The shipped catalogue has none, so this proves the check on a modified copy,
        // the way the writer would see a future catalogue.
        let mut c = personal_fact_fields::registry().unwrap().clone();
        c.fields[1].sensitivity = "financial".to_owned();
        assert_eq!(
            personal_fact_fields::check_user_choice(&c, "job_title", Tier::Two, "pass"),
            Err(ChoiceError::FloorForbidsPass)
        );
        assert!(
            personal_fact_fields::check_user_choice(&c, "job_title", Tier::Two, "omit").is_ok()
        );
    }

    #[tokio::test]
    async fn the_loaded_policies_feed_the_resolver_and_default_to_omit() {
        with_db(|mut conn| async move {
            let none = UserPolicies::load(&mut conn).await;
            assert_eq!(
                resolved(none.lookup("job_title", Tier::Two), Tier::Two),
                Policy::Omit
            );
            set_fact_policy(&mut conn, "job_title", Tier::Two, "pass", SET_BY_USER)
                .await
                .unwrap();
            let p = UserPolicies::load(&mut conn).await;
            assert_eq!(
                resolved(p.lookup("job_title", Tier::Two), Tier::Two),
                Policy::Pass
            );
            // The tier with no choice still resolves to the default.
            assert_eq!(p.lookup("job_title", Tier::Three), None);
            assert_eq!(
                resolved(p.lookup("job_title", Tier::Three), Tier::Three),
                Policy::Omit
            );
            assert_eq!(p.lookup("employer", Tier::Two), None);
        })
        .await;
    }

    #[tokio::test]
    async fn a_garbage_row_and_a_missing_table_fail_closed_to_omit() {
        with_db(|mut conn| async move {
            // A row no writer could have made: the reader still answers omit.
            sqlx::query(
                "INSERT INTO fact_disclosure_policy (field_name, tier2, tier3, set_at, set_by)
                 VALUES ('job_title', 'definitely-pass', 'not_permitted', 'now', 'user')",
            )
            .execute(&mut conn)
            .await
            .unwrap();
            let p = UserPolicies::load(&mut conn).await;
            assert_eq!(
                resolved(p.lookup("job_title", Tier::Two), Tier::Two),
                Policy::Omit
            );
            assert_eq!(
                resolved(p.lookup("job_title", Tier::Three), Tier::Three),
                Policy::Omit
            );

            sqlx::query("DROP TABLE fact_disclosure_policy")
                .execute(&mut conn)
                .await
                .unwrap();
            let p = UserPolicies::load(&mut conn).await;
            assert!(matches!(p, UserPolicies::Unreadable));
            for tier in [Tier::Two, Tier::Three] {
                assert_eq!(p.lookup("job_title", tier), Some("omit"));
                assert_eq!(resolved(p.lookup("job_title", tier), tier), Policy::Omit);
            }
        })
        .await;
    }

    #[test]
    fn only_this_module_writes_the_table_and_nothing_reads_the_abstraction_columns() {
        let this = include_str!("fact_policy_store.rs");
        let code = this.split("#[cfg(test)]").next().unwrap();
        for banned in [
            "abstraction_tier",
            "DELETE FROM",
            "UPDATE fact_disclosure_policy",
        ] {
            assert!(!code.contains(banned), "{banned}");
        }
        assert_eq!(
            code.matches("INSERT INTO fact_disclosure_policy").count(),
            2
        );
        // No other source file writes the table in production code (a comment that names
        // it is fine, and test modules may create and drop it).
        let verbs = [
            "INSERT INTO fact_disclosure_policy",
            "REPLACE INTO fact_disclosure_policy",
            "UPDATE fact_disclosure_policy",
            "DELETE FROM fact_disclosure_policy",
            "DROP TABLE fact_disclosure_policy",
        ];
        let mut hits = Vec::new();
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut stack = vec![root];
        while let Some(d) = stack.pop() {
            for e in std::fs::read_dir(d).unwrap().flatten() {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                } else if p.extension().is_some_and(|x| x == "rs")
                    && p.file_name().is_some_and(|n| n != "fact_policy_store.rs")
                {
                    let t = std::fs::read_to_string(&p).unwrap();
                    // Production code only: migration tests build and break the table.
                    let t = t.split("#[cfg(test)]").next().unwrap();
                    if verbs.iter().any(|v| t.contains(v)) {
                        hits.push(p.file_name().unwrap().to_string_lossy().into_owned());
                    }
                }
            }
        }
        assert!(hits.is_empty(), "{hits:?}");
    }
}
