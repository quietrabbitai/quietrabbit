// src-tauri/src/persistence/provider_models_store.rs
//
// items.id=711 (decisions.id=875 point 5, PROVIDER_REGISTRY_AND_TIER_MODEL_
// SPEC.md Part 8f): the per-user live model list in the encrypted
// integration_keys.db (keys_005.sql).
//
// WHY PER USER: a provider's list depends on the user's own key and plan, and
// the rows reveal which models a user touches. Nothing a refresh learns is
// written to shared.db (global, curator-owned, plaintext).
//
// A model that a refresh no longer returns is FLAGGED (present = 0), never
// deleted: "may be retired" is a flag, and a stored selection is never
// dropped silently. Curated rows are never written here.
//
// Same connection style as provider_spend_store: `*_conn` functions take a
// connection the caller holds, runtime sqlx::query() only.

use chrono::{DateTime, SecondsFormat, Utc};
use sqlx::{Row, SqliteConnection};

use crate::persistence::provider_spend_store::{
    begin_immediate, commit, rollback, SpendStoreError,
};
use crate::providers::hosted_client::LiveModel;

fn stamp(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(SecondsFormat::Micros, true)
}

/// One model a refresh has returned for a provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveModelRow {
    pub provider_id: String,
    pub model_id: String,
    pub context_tokens: Option<u32>,
    /// Date the context window was read from the provider's list.
    pub context_as_of: Option<String>,
    pub retires_at: Option<String>,
    pub first_seen_at: String,
    pub last_seen_at: String,
    /// In the latest list. False = may be retired.
    pub present: bool,
    /// The output token limit the user chose for this uncurated model.
    pub user_output_limit: Option<u32>,
}

/// The latest refresh of one provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefreshRow {
    pub refreshed_at: String,
    pub listed_count: u32,
}

/// What one refresh changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefreshSummary {
    pub listed: u32,
    /// Ids seen for the first time.
    pub new: u32,
    /// Ids that were present before and are missing from this list.
    pub newly_missing: u32,
    /// Ids that were flagged missing before and are listed again.
    pub returned: u32,
}

const LIVE_COLUMNS: &str = "provider_id, model_id, context_tokens, context_as_of, retires_at,
                first_seen_at, last_seen_at, present, user_output_limit";

fn row_to_live(r: &sqlx::sqlite::SqliteRow) -> Result<LiveModelRow, SpendStoreError> {
    let optional_u32 = |col: &str| -> Result<Option<u32>, SpendStoreError> {
        Ok(r.try_get::<Option<i64>, _>(col)?
            .and_then(|v| u32::try_from(v).ok()))
    };
    Ok(LiveModelRow {
        provider_id: r.try_get("provider_id")?,
        model_id: r.try_get("model_id")?,
        context_tokens: optional_u32("context_tokens")?,
        context_as_of: r.try_get("context_as_of")?,
        retires_at: r.try_get("retires_at")?,
        first_seen_at: r.try_get("first_seen_at")?,
        last_seen_at: r.try_get("last_seen_at")?,
        present: r.try_get::<i64, _>("present")? != 0,
        user_output_limit: optional_u32("user_output_limit")?,
    })
}

/// Record a refresh: upsert every listed model as present, flag every
/// previously-present model that is not in `models` as missing, and stamp the
/// provider's refresh row. One transaction, so a failure leaves the previous
/// list intact. `user_output_limit` is never touched.
pub async fn record_refresh_conn(
    conn: &mut SqliteConnection,
    provider_id: &str,
    models: &[LiveModel],
    now: DateTime<Utc>,
) -> Result<RefreshSummary, SpendStoreError> {
    let at = stamp(now);
    let as_of = now.date_naive().to_string();
    begin_immediate(conn).await?;
    let outcome = async {
        let before: Vec<(String, i64)> = sqlx::query_as(
            "SELECT model_id, present FROM provider_live_models WHERE provider_id = ?",
        )
        .bind(provider_id)
        .fetch_all(&mut *conn)
        .await?;
        let was_present = |id: &str| before.iter().any(|(m, p)| m == id && *p != 0);
        let was_known = |id: &str| before.iter().any(|(m, _)| m == id);

        let mut summary = RefreshSummary {
            listed: u32::try_from(models.len()).unwrap_or(u32::MAX),
            new: 0,
            newly_missing: 0,
            returned: 0,
        };
        for m in models {
            if !was_known(&m.id) {
                summary.new += 1;
            } else if !was_present(&m.id) {
                summary.returned += 1;
            }
            sqlx::query(
                "INSERT INTO provider_live_models
                    (provider_id, model_id, context_tokens, context_as_of, retires_at,
                     first_seen_at, last_seen_at, present)
                 VALUES (?, ?, ?, ?, ?, ?, ?, 1)
                 ON CONFLICT (provider_id, model_id) DO UPDATE SET
                    context_tokens = excluded.context_tokens,
                    context_as_of = excluded.context_as_of,
                    retires_at = excluded.retires_at,
                    last_seen_at = excluded.last_seen_at,
                    present = 1",
            )
            .bind(provider_id)
            .bind(&m.id)
            .bind(m.context_tokens.map(i64::from))
            .bind(m.context_tokens.map(|_| as_of.as_str()))
            .bind(m.retires_at.as_deref())
            .bind(&at)
            .bind(&at)
            .execute(&mut *conn)
            .await?;
        }
        for (id, present) in &before {
            if *present != 0 && !models.iter().any(|m| &m.id == id) {
                summary.newly_missing += 1;
                sqlx::query(
                    "UPDATE provider_live_models SET present = 0
                     WHERE provider_id = ? AND model_id = ?",
                )
                .bind(provider_id)
                .bind(id)
                .execute(&mut *conn)
                .await?;
            }
        }
        sqlx::query(
            "INSERT INTO provider_model_refresh (provider_id, refreshed_at, listed_count)
             VALUES (?, ?, ?)
             ON CONFLICT (provider_id) DO UPDATE SET
                refreshed_at = excluded.refreshed_at, listed_count = excluded.listed_count",
        )
        .bind(provider_id)
        .bind(&at)
        .bind(i64::from(summary.listed))
        .execute(&mut *conn)
        .await?;
        Ok::<_, SpendStoreError>(summary)
    }
    .await;
    match outcome {
        Ok(summary) => {
            commit(conn).await?;
            Ok(summary)
        }
        Err(e) => {
            rollback(conn).await;
            Err(e)
        }
    }
}

pub async fn list_live_models_conn(
    conn: &mut SqliteConnection,
    provider_id: &str,
) -> Result<Vec<LiveModelRow>, SpendStoreError> {
    let sql = format!(
        "SELECT {LIVE_COLUMNS} FROM provider_live_models WHERE provider_id = ? ORDER BY model_id"
    );
    let rows = sqlx::query(&sql)
        .bind(provider_id)
        .fetch_all(&mut *conn)
        .await?;
    rows.iter().map(row_to_live).collect()
}

pub async fn get_live_model_conn(
    conn: &mut SqliteConnection,
    provider_id: &str,
    model_id: &str,
) -> Result<Option<LiveModelRow>, SpendStoreError> {
    let sql = format!(
        "SELECT {LIVE_COLUMNS} FROM provider_live_models WHERE provider_id = ? AND model_id = ?"
    );
    let row = sqlx::query(&sql)
        .bind(provider_id)
        .bind(model_id)
        .fetch_optional(&mut *conn)
        .await?;
    row.as_ref().map(row_to_live).transpose()
}

pub async fn get_refresh_conn(
    conn: &mut SqliteConnection,
    provider_id: &str,
) -> Result<Option<RefreshRow>, SpendStoreError> {
    let row = sqlx::query(
        "SELECT refreshed_at, listed_count FROM provider_model_refresh WHERE provider_id = ?",
    )
    .bind(provider_id)
    .fetch_optional(&mut *conn)
    .await?;
    row.map(|r| {
        Ok(RefreshRow {
            refreshed_at: r.try_get("refreshed_at")?,
            listed_count: u32::try_from(r.try_get::<i64, _>("listed_count")?).unwrap_or(u32::MAX),
        })
    })
    .transpose()
}

/// Set (or clear) the output token limit the user chose for a live model.
/// Returns whether the model has a live row.
pub async fn set_user_output_limit_conn(
    conn: &mut SqliteConnection,
    provider_id: &str,
    model_id: &str,
    limit: Option<u32>,
) -> Result<bool, SpendStoreError> {
    let result = sqlx::query(
        "UPDATE provider_live_models SET user_output_limit = ?
         WHERE provider_id = ? AND model_id = ?",
    )
    .bind(limit.map(i64::from))
    .bind(provider_id)
    .bind(model_id)
    .execute(&mut *conn)
    .await?;
    Ok(result.rows_affected() > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persistence::provider_spend_store::open;
    use chrono::TimeZone;

    const KEY: &str = "0202020202020202020202020202020202020202020202020202020202020202";

    fn live(id: &str, ctx: Option<u32>, retires: Option<&str>) -> LiveModel {
        LiveModel {
            id: id.to_owned(),
            context_tokens: ctx,
            retires_at: retires.map(str::to_owned),
        }
    }

    fn t(day: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, day, 12, 0, 0).unwrap()
    }

    async fn with_conn<F, Fut>(f: F)
    where
        F: FnOnce(SqliteConnection) -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        let _lock = crate::test_support::ENV_MUTEX.lock().await;
        let saved = std::env::var("QR_DATA_ROOT").ok();
        let dir = tempfile::tempdir().expect("tempdir");
        std::env::set_var("QR_DATA_ROOT", dir.path());
        let conn = open("models-user", KEY).await.expect("open keys db");
        f(conn).await;
        match saved {
            Some(v) => std::env::set_var("QR_DATA_ROOT", v),
            None => std::env::remove_var("QR_DATA_ROOT"),
        }
    }

    #[tokio::test]
    async fn a_refresh_records_new_models_with_context_provenance() {
        with_conn(|mut conn| async move {
            let s = record_refresh_conn(
                &mut conn,
                "p",
                &[
                    live("a", Some(8192), Some("2027-01-01")),
                    live("b", None, None),
                ],
                t(11),
            )
            .await
            .unwrap();
            assert_eq!(
                s,
                RefreshSummary {
                    listed: 2,
                    new: 2,
                    newly_missing: 0,
                    returned: 0
                }
            );
            let a = get_live_model_conn(&mut conn, "p", "a")
                .await
                .unwrap()
                .unwrap();
            assert_eq!(a.context_tokens, Some(8192));
            assert_eq!(a.context_as_of.as_deref(), Some("2026-10-11"));
            assert_eq!(a.retires_at.as_deref(), Some("2027-01-01"));
            assert!(a.present);
            let b = get_live_model_conn(&mut conn, "p", "b")
                .await
                .unwrap()
                .unwrap();
            assert_eq!(b.context_tokens, None);
            assert_eq!(b.context_as_of, None);
            let r = get_refresh_conn(&mut conn, "p").await.unwrap().unwrap();
            assert_eq!(r.listed_count, 2);
        })
        .await;
    }

    #[tokio::test]
    async fn a_model_missing_from_a_later_list_is_flagged_not_deleted() {
        with_conn(|mut conn| async move {
            record_refresh_conn(
                &mut conn,
                "p",
                &[live("a", None, None), live("b", None, None)],
                t(11),
            )
            .await
            .unwrap();
            assert!(set_user_output_limit_conn(&mut conn, "p", "b", Some(512))
                .await
                .unwrap());

            let s = record_refresh_conn(&mut conn, "p", &[live("a", Some(100), None)], t(12))
                .await
                .unwrap();
            assert_eq!((s.new, s.newly_missing, s.returned), (0, 1, 0));
            let b = get_live_model_conn(&mut conn, "p", "b")
                .await
                .unwrap()
                .unwrap();
            assert!(!b.present, "missing means may be retired");
            assert_eq!(b.user_output_limit, Some(512), "the user's limit survives");
            assert_eq!(
                b.first_seen_at, b.last_seen_at,
                "last seen stays at the old list"
            );

            // It comes back: present again, counted as returned.
            let s = record_refresh_conn(
                &mut conn,
                "p",
                &[live("a", None, None), live("b", None, None)],
                t(13),
            )
            .await
            .unwrap();
            assert_eq!((s.new, s.newly_missing, s.returned), (0, 0, 1));
            assert!(
                get_live_model_conn(&mut conn, "p", "b")
                    .await
                    .unwrap()
                    .unwrap()
                    .present
            );
        })
        .await;
    }

    #[tokio::test]
    async fn a_refresh_only_touches_its_own_provider() {
        with_conn(|mut conn| async move {
            record_refresh_conn(&mut conn, "p1", &[live("a", None, None)], t(11))
                .await
                .unwrap();
            record_refresh_conn(&mut conn, "p2", &[live("z", None, None)], t(11))
                .await
                .unwrap();
            record_refresh_conn(&mut conn, "p1", &[], t(12))
                .await
                .unwrap();
            assert!(
                !get_live_model_conn(&mut conn, "p1", "a")
                    .await
                    .unwrap()
                    .unwrap()
                    .present
            );
            assert!(
                get_live_model_conn(&mut conn, "p2", "z")
                    .await
                    .unwrap()
                    .unwrap()
                    .present
            );
            assert_eq!(
                list_live_models_conn(&mut conn, "p2").await.unwrap().len(),
                1
            );
        })
        .await;
    }

    #[tokio::test]
    async fn context_and_retirement_update_when_the_provider_changes_them() {
        with_conn(|mut conn| async move {
            record_refresh_conn(
                &mut conn,
                "p",
                &[live("a", Some(100), Some("2027-01-01"))],
                t(11),
            )
            .await
            .unwrap();
            record_refresh_conn(&mut conn, "p", &[live("a", Some(200), None)], t(12))
                .await
                .unwrap();
            let a = get_live_model_conn(&mut conn, "p", "a")
                .await
                .unwrap()
                .unwrap();
            assert_eq!(a.context_tokens, Some(200));
            assert_eq!(a.context_as_of.as_deref(), Some("2026-10-12"));
            assert_eq!(a.retires_at, None);
        })
        .await;
    }

    #[tokio::test]
    async fn the_output_limit_is_only_set_on_a_known_model() {
        with_conn(|mut conn| async move {
            assert!(
                !set_user_output_limit_conn(&mut conn, "p", "ghost", Some(5))
                    .await
                    .unwrap()
            );
            assert!(get_refresh_conn(&mut conn, "p").await.unwrap().is_none());
        })
        .await;
    }
}
