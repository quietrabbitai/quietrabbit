// src-tauri/src/persistence/provider_spend_store.rs
//
// items.id=710 (decisions.id=875 point 3, PROVIDER_REGISTRY_AND_TIER_MODEL_
// SPEC.md Part 8c-8e): reads and writes for the per-user spend tables in the
// encrypted integration_keys.db (keys_003.sql, keys_004.sql): consent, limits,
// the usage ledger, the rate-limit snapshot, price overrides and calibration.
//
// WHY ENCRYPTED, PER USER: these rows reveal which providers a user uses and
// how much they spend (keys_003.sql header, decisions.id=862). They never live
// in shared.db.
//
// CONNECTION STYLE: single connection on demand per call, opened by
// integration_keys_store::open_integration_keys_db (PRAGMA key before
// journal_mode, migrator always run). Functions named `*_conn` take a
// connection the caller already holds, so the pre-flight can do its reads and
// its reservation inside ONE `BEGIN IMMEDIATE` transaction (spend::gate).
// Runtime sqlx::query() only, never the query!() macros.
//
// MONEY: integer micro-USD only (i64). No floats anywhere in this file; the
// REAL calibration columns are read as text-free integers by the caller's
// conversion (`spend::money::permille_ceil`), not here.
//
// TIMESTAMPS: every `recorded_at` this module writes is
// `ledger_timestamp(...)`, a FIXED-WIDTH UTC form (microseconds, trailing Z),
// so lexicographic comparison against a period boundary formatted the same way
// is exactly chronological comparison.

use chrono::{DateTime, Datelike, Duration, LocalResult, NaiveDate, SecondsFormat, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{Row, SqliteConnection};

use crate::persistence::integration_keys_store::{
    open_integration_keys_db, IntegrationKeysStoreError,
};
use crate::providers::hosted_client::RateLimitInfo;

pub type SpendStoreError = IntegrationKeysStoreError;

fn validation(msg: impl Into<String>) -> SpendStoreError {
    IntegrationKeysStoreError::Validation(msg.into())
}

/// A reservation older than this is treated as abandoned by a crash. It is
/// longer than the client's 60 second generate timeout plus the retry waits
/// a single call can sit through.
pub const STALE_PENDING_MINUTES: i64 = 5;

// ---------------------------------------------------------------------------
// Vocabulary
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "snake_case")]
pub enum LimitMetric {
    CostUsd,
    TokensIn,
    TokensOut,
    Requests,
}

impl LimitMetric {
    pub fn as_str(self) -> &'static str {
        match self {
            LimitMetric::CostUsd => "cost_usd",
            LimitMetric::TokensIn => "tokens_in",
            LimitMetric::TokensOut => "tokens_out",
            LimitMetric::Requests => "requests",
        }
    }
    fn parse(s: &str) -> Result<Self, SpendStoreError> {
        match s {
            "cost_usd" => Ok(LimitMetric::CostUsd),
            "tokens_in" => Ok(LimitMetric::TokensIn),
            "tokens_out" => Ok(LimitMetric::TokensOut),
            "requests" => Ok(LimitMetric::Requests),
            other => Err(validation(format!("unknown limit metric '{other}'"))),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "snake_case")]
pub enum LimitPeriod {
    Run,
    Day,
    Week,
    Month,
}

impl LimitPeriod {
    pub fn as_str(self) -> &'static str {
        match self {
            LimitPeriod::Run => "run",
            LimitPeriod::Day => "day",
            LimitPeriod::Week => "week",
            LimitPeriod::Month => "month",
        }
    }
    fn parse(s: &str) -> Result<Self, SpendStoreError> {
        match s {
            "run" => Ok(LimitPeriod::Run),
            "day" => Ok(LimitPeriod::Day),
            "week" => Ok(LimitPeriod::Week),
            "month" => Ok(LimitPeriod::Month),
            other => Err(validation(format!("unknown limit period '{other}'"))),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "snake_case")]
pub enum ScopeKind {
    All,
    Provider,
    Persona,
    Focus,
}

impl ScopeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ScopeKind::All => "all",
            ScopeKind::Provider => "provider",
            ScopeKind::Persona => "persona",
            ScopeKind::Focus => "focus",
        }
    }
    fn parse(s: &str) -> Result<Self, SpendStoreError> {
        match s {
            "all" => Ok(ScopeKind::All),
            "provider" => Ok(ScopeKind::Provider),
            "persona" => Ok(ScopeKind::Persona),
            "focus" => Ok(ScopeKind::Focus),
            other => Err(validation(format!("unknown limit scope '{other}'"))),
        }
    }
}

/// How a ledger row was settled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LedgerOutcome {
    Pending,
    Ok,
    Truncated,
    BudgetExhausted,
    /// The provider answered with an error status. Nothing was generated.
    Failed,
    /// The request may have reached the provider but no usage came back (a
    /// read timeout, a reset), or a reservation was abandoned by a crash.
    /// Charged at the reserved worst case.
    Interrupted,
}

impl LedgerOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            LedgerOutcome::Pending => "pending",
            LedgerOutcome::Ok => "ok",
            LedgerOutcome::Truncated => "truncated",
            LedgerOutcome::BudgetExhausted => "budget_exhausted",
            LedgerOutcome::Failed => "failed",
            LedgerOutcome::Interrupted => "interrupted",
        }
    }
}

/// Where a settled row's token counts came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageSource {
    /// The provider's own usage counts.
    Reported,
    /// QR's estimate, because a successful response carried no usage.
    Estimated,
    /// No usage at all (a failed call, or an interrupted one charged at the
    /// reserved worst case).
    None,
}

impl UsageSource {
    pub fn as_str(self) -> &'static str {
        match self {
            UsageSource::Reported => "reported",
            UsageSource::Estimated => "estimated",
            UsageSource::None => "none",
        }
    }
}

// ---------------------------------------------------------------------------
// Time
// ---------------------------------------------------------------------------

/// The fixed-width UTC form every ledger timestamp uses.
pub fn ledger_timestamp(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(SecondsFormat::Micros, true)
}

/// Local midnight of `date` in `tz` as UTC. A day whose midnight does not
/// exist (a DST gap at 00:00) starts at the first valid hour after it.
fn local_day_start<Tz: TimeZone>(tz: &Tz, date: NaiveDate) -> Option<DateTime<Utc>> {
    for hour in 0..=3 {
        let naive = date.and_hms_opt(hour, 0, 0)?;
        match tz.from_local_datetime(&naive) {
            LocalResult::Single(dt) => return Some(dt.with_timezone(&Utc)),
            LocalResult::Ambiguous(first, _) => return Some(first.with_timezone(&Utc)),
            LocalResult::None => continue,
        }
    }
    None
}

/// The start of the calendar window containing `now` in `tz` (the user's
/// local time in production): local midnight for a day, the ISO Monday for a
/// week, the first of the month for a month. `Run` has no time window.
pub fn period_start<Tz: TimeZone>(
    period: LimitPeriod,
    now: DateTime<Utc>,
    tz: &Tz,
) -> Option<DateTime<Utc>> {
    let local_date = now.with_timezone(tz).date_naive();
    let start_date = match period {
        LimitPeriod::Run => return None,
        LimitPeriod::Day => local_date,
        LimitPeriod::Week => {
            let back = i64::from(local_date.weekday().num_days_from_monday());
            local_date - Duration::days(back)
        }
        LimitPeriod::Month => local_date.with_day(1)?,
    };
    local_day_start(tz, start_date)
}

// ---------------------------------------------------------------------------
// Consent
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsentRow {
    pub id: String,
    pub provider_id: String,
    pub attributes_hash: String,
    pub consented_at: String,
    pub revoked_at: Option<String>,
    pub no_cap_confirmed: bool,
}

fn row_to_consent(r: &sqlx::sqlite::SqliteRow) -> Result<ConsentRow, sqlx::Error> {
    Ok(ConsentRow {
        id: r.try_get("id")?,
        provider_id: r.try_get("provider_id")?,
        attributes_hash: r.try_get("attributes_hash")?,
        consented_at: r.try_get("consented_at")?,
        revoked_at: r.try_get("revoked_at")?,
        no_cap_confirmed: r.try_get::<i64, _>("no_cap_confirmed")? != 0,
    })
}

/// Where a provider's consent stands: the latest row that is not revoked, and
/// whether any row exists at all (to tell "never consented" from "revoked").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsentStanding {
    pub current: Option<ConsentRow>,
    pub any_row: bool,
}

pub async fn consent_standing_conn(
    conn: &mut SqliteConnection,
    provider_id: &str,
) -> Result<ConsentStanding, SpendStoreError> {
    let row = sqlx::query(
        "SELECT id, provider_id, attributes_hash, consented_at, revoked_at, no_cap_confirmed
         FROM provider_consent
         WHERE provider_id = ? AND revoked_at IS NULL
         ORDER BY consented_at DESC, rowid DESC LIMIT 1",
    )
    .bind(provider_id)
    .fetch_optional(&mut *conn)
    .await?;
    let current = row.map(|r| row_to_consent(&r)).transpose()?;
    let any_row: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM provider_consent WHERE provider_id = ?")
            .bind(provider_id)
            .fetch_one(&mut *conn)
            .await?;
    Ok(ConsentStanding {
        current,
        any_row: any_row > 0,
    })
}

pub async fn insert_consent_conn(
    conn: &mut SqliteConnection,
    provider_id: &str,
    attributes_hash: &str,
    no_cap_confirmed: bool,
    now: DateTime<Utc>,
) -> Result<String, SpendStoreError> {
    if provider_id.trim().is_empty() || attributes_hash.trim().is_empty() {
        return Err(validation(
            "consent needs a provider and an attributes hash",
        ));
    }
    let id = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO provider_consent
            (id, provider_id, attributes_hash, consented_at, revoked_at, no_cap_confirmed)
         VALUES (?, ?, ?, ?, NULL, ?)",
    )
    .bind(&id)
    .bind(provider_id)
    .bind(attributes_hash)
    .bind(ledger_timestamp(now))
    .bind(i64::from(no_cap_confirmed))
    .execute(&mut *conn)
    .await?;
    Ok(id)
}

/// Revoke: stamps `revoked_at` on every not-yet-revoked row of the provider.
/// That is the only mutation a consent row ever gets. Returns how many rows
/// were stamped.
pub async fn revoke_consent_conn(
    conn: &mut SqliteConnection,
    provider_id: &str,
    now: DateTime<Utc>,
) -> Result<u64, SpendStoreError> {
    let r = sqlx::query(
        "UPDATE provider_consent SET revoked_at = ?
         WHERE provider_id = ? AND revoked_at IS NULL",
    )
    .bind(ledger_timestamp(now))
    .bind(provider_id)
    .execute(&mut *conn)
    .await?;
    Ok(r.rows_affected())
}

// ---------------------------------------------------------------------------
// Limits
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LimitSpec {
    pub metric: LimitMetric,
    pub period: LimitPeriod,
    pub scope_kind: ScopeKind,
    pub scope_id: Option<String>,
    /// Micro-USD for cost, a plain count for the other metrics.
    pub limit_value: i64,
    pub warn_at_pct: u8,
    pub approves_unattended: bool,
}

impl LimitSpec {
    pub fn validate(&self) -> Result<(), SpendStoreError> {
        if self.limit_value <= 0 {
            return Err(validation("a limit must be greater than zero"));
        }
        if !(1..=100).contains(&self.warn_at_pct) {
            return Err(validation("the warning threshold must be 1 to 100 percent"));
        }
        match (self.scope_kind, &self.scope_id) {
            (ScopeKind::All, None) => {}
            (ScopeKind::All, Some(_)) => {
                return Err(validation("an all-providers limit takes no scope id"))
            }
            (_, Some(id)) if !id.trim().is_empty() => {}
            (_, _) => return Err(validation("this scope needs an id")),
        }
        if self.approves_unattended
            && !(self.metric == LimitMetric::CostUsd && self.period == LimitPeriod::Run)
        {
            return Err(validation(
                "only a per-run cost limit can approve unattended spending",
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LimitRow {
    pub id: String,
    pub spec: LimitSpec,
    pub created_at: String,
    pub updated_at: String,
}

const LIMIT_COLUMNS: &str = "id, metric, period, scope_kind, scope_id, limit_value,
        warn_at_pct, approves_unattended, created_at, updated_at";

fn row_to_limit(r: &sqlx::sqlite::SqliteRow) -> Result<LimitRow, SpendStoreError> {
    let warn: i64 = r.try_get("warn_at_pct")?;
    Ok(LimitRow {
        id: r.try_get("id")?,
        spec: LimitSpec {
            metric: LimitMetric::parse(&r.try_get::<String, _>("metric")?)?,
            period: LimitPeriod::parse(&r.try_get::<String, _>("period")?)?,
            scope_kind: ScopeKind::parse(&r.try_get::<String, _>("scope_kind")?)?,
            scope_id: r.try_get("scope_id")?,
            limit_value: r.try_get("limit_value")?,
            warn_at_pct: u8::try_from(warn)
                .map_err(|_| validation("stored warn_at_pct invalid"))?,
            approves_unattended: r.try_get::<i64, _>("approves_unattended")? != 0,
        },
        created_at: r.try_get("created_at")?,
        updated_at: r.try_get("updated_at")?,
    })
}

/// Insert or update the one limit with this (metric, period, scope).
pub async fn upsert_limit_conn(
    conn: &mut SqliteConnection,
    spec: &LimitSpec,
    now: DateTime<Utc>,
) -> Result<LimitRow, SpendStoreError> {
    spec.validate()?;
    let stamp = ledger_timestamp(now);
    let existing: Option<String> = sqlx::query_scalar(
        "SELECT id FROM provider_limits
         WHERE metric = ? AND period = ? AND scope_kind = ? AND COALESCE(scope_id, '') = ?",
    )
    .bind(spec.metric.as_str())
    .bind(spec.period.as_str())
    .bind(spec.scope_kind.as_str())
    .bind(spec.scope_id.as_deref().unwrap_or(""))
    .fetch_optional(&mut *conn)
    .await?;
    let id = match existing {
        Some(id) => {
            sqlx::query(
                "UPDATE provider_limits
                 SET limit_value = ?, warn_at_pct = ?, approves_unattended = ?, updated_at = ?
                 WHERE id = ?",
            )
            .bind(spec.limit_value)
            .bind(i64::from(spec.warn_at_pct))
            .bind(i64::from(spec.approves_unattended))
            .bind(&stamp)
            .bind(&id)
            .execute(&mut *conn)
            .await?;
            id
        }
        None => {
            let id = uuid::Uuid::new_v4().to_string();
            sqlx::query(
                "INSERT INTO provider_limits
                    (id, metric, period, scope_kind, scope_id, limit_value, warn_at_pct,
                     approves_unattended, created_at, updated_at)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(&id)
            .bind(spec.metric.as_str())
            .bind(spec.period.as_str())
            .bind(spec.scope_kind.as_str())
            .bind(spec.scope_id.as_deref())
            .bind(spec.limit_value)
            .bind(i64::from(spec.warn_at_pct))
            .bind(i64::from(spec.approves_unattended))
            .bind(&stamp)
            .bind(&stamp)
            .execute(&mut *conn)
            .await?;
            id
        }
    };
    let sql = format!("SELECT {LIMIT_COLUMNS} FROM provider_limits WHERE id = ?");
    let row = sqlx::query(&sql).bind(&id).fetch_one(&mut *conn).await?;
    row_to_limit(&row)
}

/// Remove the limit with this (metric, period, scope). Returns whether one
/// existed.
pub async fn remove_limit_conn(
    conn: &mut SqliteConnection,
    metric: LimitMetric,
    period: LimitPeriod,
    scope_kind: ScopeKind,
    scope_id: Option<&str>,
) -> Result<bool, SpendStoreError> {
    let r = sqlx::query(
        "DELETE FROM provider_limits
         WHERE metric = ? AND period = ? AND scope_kind = ? AND COALESCE(scope_id, '') = ?",
    )
    .bind(metric.as_str())
    .bind(period.as_str())
    .bind(scope_kind.as_str())
    .bind(scope_id.unwrap_or(""))
    .execute(&mut *conn)
    .await?;
    Ok(r.rows_affected() > 0)
}

pub async fn list_limits_conn(
    conn: &mut SqliteConnection,
) -> Result<Vec<LimitRow>, SpendStoreError> {
    let sql = format!(
        "SELECT {LIMIT_COLUMNS} FROM provider_limits
         ORDER BY scope_kind, COALESCE(scope_id, ''), metric, period"
    );
    let rows = sqlx::query(&sql).fetch_all(&mut *conn).await?;
    rows.iter().map(row_to_limit).collect()
}

/// The limits that govern a call: scope all, this provider, this Persona,
/// this Focus.
pub async fn applicable_limits_conn(
    conn: &mut SqliteConnection,
    provider_id: &str,
    persona_id: Option<&str>,
    focus_id: Option<&str>,
) -> Result<Vec<LimitRow>, SpendStoreError> {
    let sql = format!(
        "SELECT {LIMIT_COLUMNS} FROM provider_limits
         WHERE scope_kind = 'all'
            OR (scope_kind = 'provider' AND scope_id = ?)
            OR (scope_kind = 'persona'  AND scope_id = ?)
            OR (scope_kind = 'focus'    AND scope_id = ?)
         ORDER BY scope_kind, COALESCE(scope_id, ''), metric, period"
    );
    let rows = sqlx::query(&sql)
        .bind(provider_id)
        .bind(persona_id)
        .bind(focus_id)
        .fetch_all(&mut *conn)
        .await?;
    rows.iter().map(row_to_limit).collect()
}

// ---------------------------------------------------------------------------
// Ledger
// ---------------------------------------------------------------------------

/// Sums over ledger rows. A pending reservation counts at its reserved
/// amounts (they sit in the ordinary columns), so concurrent calls see each
/// other.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UsageTotals {
    pub cost_micros: i64,
    pub tokens_in: i64,
    pub tokens_out: i64,
    pub requests: i64,
}

/// What to sum. `None` fields do not filter.
#[derive(Debug, Clone, Default)]
pub struct UsageFilter<'a> {
    pub provider_id: Option<&'a str>,
    pub persona_id: Option<&'a str>,
    pub focus_id: Option<&'a str>,
    pub run_id: Option<&'a str>,
    /// Inclusive lower bound, a `ledger_timestamp` string.
    pub since: Option<&'a str>,
}

pub async fn usage_totals_conn(
    conn: &mut SqliteConnection,
    filter: &UsageFilter<'_>,
) -> Result<UsageTotals, SpendStoreError> {
    let row = sqlx::query(
        "SELECT COALESCE(SUM(cost_micros), 0) AS cost,
                COALESCE(SUM(tokens_in), 0)   AS tin,
                COALESCE(SUM(tokens_out), 0)  AS tout,
                COALESCE(SUM(requests), 0)    AS reqs
         FROM provider_usage_ledger
         WHERE (? IS NULL OR provider_id = ?)
           AND (? IS NULL OR persona_id = ?)
           AND (? IS NULL OR focus_id = ?)
           AND (? IS NULL OR run_id = ?)
           AND (? IS NULL OR recorded_at >= ?)",
    )
    .bind(filter.provider_id)
    .bind(filter.provider_id)
    .bind(filter.persona_id)
    .bind(filter.persona_id)
    .bind(filter.focus_id)
    .bind(filter.focus_id)
    .bind(filter.run_id)
    .bind(filter.run_id)
    .bind(filter.since)
    .bind(filter.since)
    .fetch_one(&mut *conn)
    .await?;
    Ok(UsageTotals {
        cost_micros: row.try_get("cost")?,
        tokens_in: row.try_get("tin")?,
        tokens_out: row.try_get("tout")?,
        requests: row.try_get("reqs")?,
    })
}

/// Per-provider totals since a boundary, for the usage summary.
pub async fn usage_by_provider_conn(
    conn: &mut SqliteConnection,
    since: Option<&str>,
) -> Result<Vec<(String, UsageTotals)>, SpendStoreError> {
    let rows = sqlx::query(
        "SELECT provider_id,
                COALESCE(SUM(cost_micros), 0) AS cost,
                COALESCE(SUM(tokens_in), 0)   AS tin,
                COALESCE(SUM(tokens_out), 0)  AS tout,
                COALESCE(SUM(requests), 0)    AS reqs
         FROM provider_usage_ledger
         WHERE (? IS NULL OR recorded_at >= ?)
         GROUP BY provider_id ORDER BY provider_id",
    )
    .bind(since)
    .bind(since)
    .fetch_all(&mut *conn)
    .await?;
    rows.iter()
        .map(|r| {
            Ok((
                r.try_get("provider_id")?,
                UsageTotals {
                    cost_micros: r.try_get("cost")?,
                    tokens_in: r.try_get("tin")?,
                    tokens_out: r.try_get("tout")?,
                    requests: r.try_get("reqs")?,
                },
            ))
        })
        .collect()
}

/// What a pre-flight reserves before the network call.
#[derive(Debug, Clone)]
pub struct NewReservation<'a> {
    pub provider_id: &'a str,
    pub model_id: &'a str,
    pub persona_id: Option<&'a str>,
    pub focus_id: Option<&'a str>,
    pub run_id: Option<&'a str>,
    /// Estimated input tokens.
    pub tokens_in: i64,
    /// The output token limit (the most the call can generate).
    pub tokens_out: i64,
    /// Worst-case cost in micro-USD.
    pub cost_micros: i64,
    /// `provider_api`, `curated`, `user`, or `unknown`.
    pub price_source: &'a str,
}

pub async fn insert_reservation_conn(
    conn: &mut SqliteConnection,
    r: &NewReservation<'_>,
    now: DateTime<Utc>,
) -> Result<String, SpendStoreError> {
    let id = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO provider_usage_ledger
            (id, recorded_at, provider_id, model_id, persona_id, focus_id, run_id,
             tokens_in, tokens_out, requests, cost_micros, price_source,
             reasoning_tokens, outcome, usage_source, reserved_micros)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, 1, ?, ?, NULL, 'pending', 'none', ?)",
    )
    .bind(&id)
    .bind(ledger_timestamp(now))
    .bind(r.provider_id)
    .bind(r.model_id)
    .bind(r.persona_id)
    .bind(r.focus_id)
    .bind(r.run_id)
    .bind(r.tokens_in)
    .bind(r.tokens_out)
    .bind(r.cost_micros)
    .bind(r.price_source)
    .bind(r.cost_micros)
    .execute(&mut *conn)
    .await?;
    Ok(id)
}

/// The final figures for a reservation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settlement {
    pub tokens_in: i64,
    pub tokens_out: i64,
    pub reasoning_tokens: Option<i64>,
    pub cost_micros: i64,
    pub outcome: LedgerOutcome,
    pub usage_source: UsageSource,
}

/// Overwrite a pending reservation with what actually happened. Only a
/// still-pending row is touched; settling twice is a no-op. Returns whether a
/// row changed.
pub async fn settle_conn(
    conn: &mut SqliteConnection,
    id: &str,
    s: &Settlement,
) -> Result<bool, SpendStoreError> {
    let r = sqlx::query(
        "UPDATE provider_usage_ledger
         SET tokens_in = ?, tokens_out = ?, reasoning_tokens = ?, cost_micros = ?,
             outcome = ?, usage_source = ?
         WHERE id = ? AND outcome = 'pending'",
    )
    .bind(s.tokens_in)
    .bind(s.tokens_out)
    .bind(s.reasoning_tokens)
    .bind(s.cost_micros)
    .bind(s.outcome.as_str())
    .bind(s.usage_source.as_str())
    .bind(id)
    .execute(&mut *conn)
    .await?;
    Ok(r.rows_affected() > 0)
}

/// A call that may have reached the provider but returned no usage: keep the
/// reserved amounts (charged at the worst case) and say so.
pub async fn mark_interrupted_conn(
    conn: &mut SqliteConnection,
    id: &str,
) -> Result<bool, SpendStoreError> {
    let r = sqlx::query(
        "UPDATE provider_usage_ledger
         SET outcome = 'interrupted', usage_source = 'none'
         WHERE id = ? AND outcome = 'pending'",
    )
    .bind(id)
    .execute(&mut *conn)
    .await?;
    Ok(r.rows_affected() > 0)
}

/// Drop a reservation for a call that never left the machine.
pub async fn release_reservation_conn(
    conn: &mut SqliteConnection,
    id: &str,
) -> Result<bool, SpendStoreError> {
    let r = sqlx::query("DELETE FROM provider_usage_ledger WHERE id = ? AND outcome = 'pending'")
        .bind(id)
        .execute(&mut *conn)
        .await?;
    Ok(r.rows_affected() > 0)
}

/// Startup recovery: reservations left pending by a crash become interrupted
/// and keep their reserved amounts. Returns how many were converted.
pub async fn recover_interrupted_conn(
    conn: &mut SqliteConnection,
    now: DateTime<Utc>,
) -> Result<u64, SpendStoreError> {
    let cutoff = ledger_timestamp(now - Duration::minutes(STALE_PENDING_MINUTES));
    let r = sqlx::query(
        "UPDATE provider_usage_ledger
         SET outcome = 'interrupted', usage_source = 'none'
         WHERE outcome = 'pending' AND recorded_at < ?",
    )
    .bind(cutoff)
    .execute(&mut *conn)
    .await?;
    Ok(r.rows_affected())
}

/// Best-effort login step: open the user's keys store and recover. Never
/// fails the caller; returns the converted count (0 on any problem).
pub async fn recover_interrupted_spend(user_id: &str, key_hex: &str) -> u64 {
    let mut conn = match open_integration_keys_db(user_id, key_hex).await {
        Ok(c) => c,
        Err(e) => {
            log::warn!("spend recovery: could not open the keys store: {e}");
            return 0;
        }
    };
    match recover_interrupted_conn(&mut conn, Utc::now()).await {
        Ok(n) => {
            if n > 0 {
                log::info!(
                    "spend recovery: {n} abandoned reservation(s) charged at their worst case"
                );
            }
            n
        }
        Err(e) => {
            log::warn!("spend recovery failed: {e}");
            0
        }
    }
}

// ---------------------------------------------------------------------------
// Rate-limit snapshot
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RateSnapshot {
    pub provider_id: String,
    pub observed_at: String,
    pub requests_limit: Option<i64>,
    pub requests_remaining: Option<i64>,
    pub tokens_limit: Option<i64>,
    pub tokens_remaining: Option<i64>,
    pub requests_reset: Option<String>,
    pub tokens_reset: Option<String>,
}

fn as_i64(v: Option<u64>) -> Option<i64> {
    v.and_then(|n| i64::try_from(n).ok())
}

/// Replace the provider's latest rate-limit observation. Nothing is written
/// when the response carried no rate-limit headers (an old snapshot is not
/// overwritten by silence).
pub async fn upsert_rate_snapshot_conn(
    conn: &mut SqliteConnection,
    provider_id: &str,
    info: &RateLimitInfo,
    now: DateTime<Utc>,
) -> Result<bool, SpendStoreError> {
    if info.is_empty() {
        return Ok(false);
    }
    let raw: serde_json::Map<String, serde_json::Value> = info
        .entries()
        .iter()
        .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
        .collect();
    sqlx::query(
        "INSERT INTO provider_rate_snapshot
            (provider_id, observed_at, requests_limit, requests_remaining, tokens_limit,
             tokens_remaining, requests_reset, tokens_reset, raw)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT(provider_id) DO UPDATE SET
            observed_at = excluded.observed_at,
            requests_limit = excluded.requests_limit,
            requests_remaining = excluded.requests_remaining,
            tokens_limit = excluded.tokens_limit,
            tokens_remaining = excluded.tokens_remaining,
            requests_reset = excluded.requests_reset,
            tokens_reset = excluded.tokens_reset,
            raw = excluded.raw",
    )
    .bind(provider_id)
    .bind(ledger_timestamp(now))
    .bind(as_i64(info.limit_requests()))
    .bind(as_i64(info.remaining_requests()))
    .bind(as_i64(info.limit_tokens()))
    .bind(as_i64(info.remaining_tokens()))
    .bind(info.reset_requests())
    .bind(info.reset_tokens())
    .bind(serde_json::Value::Object(raw).to_string())
    .execute(&mut *conn)
    .await?;
    Ok(true)
}

pub async fn get_rate_snapshot_conn(
    conn: &mut SqliteConnection,
    provider_id: &str,
) -> Result<Option<RateSnapshot>, SpendStoreError> {
    let row = sqlx::query(
        "SELECT provider_id, observed_at, requests_limit, requests_remaining, tokens_limit,
                tokens_remaining, requests_reset, tokens_reset
         FROM provider_rate_snapshot WHERE provider_id = ?",
    )
    .bind(provider_id)
    .fetch_optional(&mut *conn)
    .await?;
    row.map(|r| {
        Ok(RateSnapshot {
            provider_id: r.try_get("provider_id")?,
            observed_at: r.try_get("observed_at")?,
            requests_limit: r.try_get("requests_limit")?,
            requests_remaining: r.try_get("requests_remaining")?,
            tokens_limit: r.try_get("tokens_limit")?,
            tokens_remaining: r.try_get("tokens_remaining")?,
            requests_reset: r.try_get("requests_reset")?,
            tokens_reset: r.try_get("tokens_reset")?,
        })
    })
    .transpose()
}

// ---------------------------------------------------------------------------
// Price overrides and calibration (items.id=710 read side, items.id=711 writers)
// ---------------------------------------------------------------------------

/// The user's raw price-override JSON for a model, if any.
pub async fn get_price_override_raw_conn(
    conn: &mut SqliteConnection,
    provider_id: &str,
    model_id: &str,
) -> Result<Option<String>, SpendStoreError> {
    Ok(sqlx::query_scalar(
        "SELECT pricing FROM price_overrides WHERE provider_id = ? AND model_id = ?",
    )
    .bind(provider_id)
    .bind(model_id)
    .fetch_optional(&mut *conn)
    .await?)
}

/// The user's calibration factors for a model as integer permille (1000 =
/// neutral). The factors are REAL columns; they are read as text and
/// converted by the caller's `permille_ceil`, so no float is handled here.
#[derive(Debug, Clone, PartialEq)]
pub struct CalibrationText {
    pub input_factor: String,
    pub output_factor: String,
    /// Input samples (keys_003.sql `sample_count`).
    pub sample_count: i64,
    /// Output (reasoning) samples (keys_005.sql).
    pub output_sample_count: i64,
}

pub async fn get_calibration_conn(
    conn: &mut SqliteConnection,
    provider_id: &str,
    model_id: &str,
) -> Result<Option<CalibrationText>, SpendStoreError> {
    let row = sqlx::query(
        "SELECT CAST(input_factor AS TEXT) AS inf, CAST(output_factor AS TEXT) AS outf,
                sample_count, output_sample_count
         FROM calibration WHERE provider_id = ? AND model_id = ?",
    )
    .bind(provider_id)
    .bind(model_id)
    .fetch_optional(&mut *conn)
    .await?;
    row.map(|r| {
        Ok(CalibrationText {
            input_factor: r.try_get("inf")?,
            output_factor: r.try_get("outf")?,
            sample_count: r.try_get("sample_count")?,
            output_sample_count: r.try_get("output_sample_count")?,
        })
    })
    .transpose()
}

/// A calibration row to write (items.id=711). The factors are plain decimal
/// text (for example `1.234`, produced from integer permille) and are stored
/// as REAL by SQLite itself, so no float is handled in Rust.
pub struct CalibrationWrite<'a> {
    pub provider_id: &'a str,
    pub model_id: &'a str,
    pub input_factor: &'a str,
    pub output_factor: &'a str,
    pub sample_count: i64,
    pub output_sample_count: i64,
}

pub async fn upsert_calibration_conn(
    conn: &mut SqliteConnection,
    row: &CalibrationWrite<'_>,
    now: DateTime<Utc>,
) -> Result<(), SpendStoreError> {
    sqlx::query(
        "INSERT INTO calibration
            (provider_id, model_id, input_factor, output_factor, sample_count,
             output_sample_count, updated_at)
         VALUES (?, ?, CAST(? AS REAL), CAST(? AS REAL), ?, ?, ?)
         ON CONFLICT (provider_id, model_id) DO UPDATE SET
            input_factor = excluded.input_factor,
            output_factor = excluded.output_factor,
            sample_count = excluded.sample_count,
            output_sample_count = excluded.output_sample_count,
            updated_at = excluded.updated_at",
    )
    .bind(row.provider_id)
    .bind(row.model_id)
    .bind(row.input_factor)
    .bind(row.output_factor)
    .bind(row.sample_count)
    .bind(row.output_sample_count)
    .bind(ledger_timestamp(now))
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Store the user's price override for a model: JSON of the same shape as
/// `provider_models.pricing` with source `user` (keys_003.sql).
pub async fn upsert_price_override_conn(
    conn: &mut SqliteConnection,
    provider_id: &str,
    model_id: &str,
    pricing_json: &str,
    now: DateTime<Utc>,
) -> Result<(), SpendStoreError> {
    sqlx::query(
        "INSERT INTO price_overrides (provider_id, model_id, pricing, updated_at)
         VALUES (?, ?, ?, ?)
         ON CONFLICT (provider_id, model_id) DO UPDATE SET
            pricing = excluded.pricing, updated_at = excluded.updated_at",
    )
    .bind(provider_id)
    .bind(model_id)
    .bind(pricing_json)
    .bind(ledger_timestamp(now))
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Remove the user's price override. Returns whether one existed.
pub async fn delete_price_override_conn(
    conn: &mut SqliteConnection,
    provider_id: &str,
    model_id: &str,
) -> Result<bool, SpendStoreError> {
    let result = sqlx::query("DELETE FROM price_overrides WHERE provider_id = ? AND model_id = ?")
        .bind(provider_id)
        .bind(model_id)
        .execute(&mut *conn)
        .await?;
    Ok(result.rows_affected() > 0)
}

/// Every price override for one provider as `(model_id, raw pricing JSON)`.
pub async fn list_price_overrides_conn(
    conn: &mut SqliteConnection,
    provider_id: &str,
) -> Result<Vec<(String, String)>, SpendStoreError> {
    Ok(sqlx::query_as(
        "SELECT model_id, pricing FROM price_overrides WHERE provider_id = ? ORDER BY model_id",
    )
    .bind(provider_id)
    .fetch_all(&mut *conn)
    .await?)
}

/// Every calibration row for one provider, keyed by model id.
pub async fn list_calibrations_conn(
    conn: &mut SqliteConnection,
    provider_id: &str,
) -> Result<Vec<(String, CalibrationText)>, SpendStoreError> {
    let rows = sqlx::query(
        "SELECT model_id, CAST(input_factor AS TEXT) AS inf, CAST(output_factor AS TEXT) AS outf,
                sample_count, output_sample_count
         FROM calibration WHERE provider_id = ? ORDER BY model_id",
    )
    .bind(provider_id)
    .fetch_all(&mut *conn)
    .await?;
    rows.iter()
        .map(|r| {
            Ok((
                r.try_get("model_id")?,
                CalibrationText {
                    input_factor: r.try_get("inf")?,
                    output_factor: r.try_get("outf")?,
                    sample_count: r.try_get("sample_count")?,
                    output_sample_count: r.try_get("output_sample_count")?,
                },
            ))
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Transactions
// ---------------------------------------------------------------------------

/// Start a write transaction up front, so the pre-flight's reads and its
/// reservation cannot interleave with another call's.
pub async fn begin_immediate(conn: &mut SqliteConnection) -> Result<(), SpendStoreError> {
    sqlx::query("BEGIN IMMEDIATE").execute(&mut *conn).await?;
    Ok(())
}

pub async fn commit(conn: &mut SqliteConnection) -> Result<(), SpendStoreError> {
    sqlx::query("COMMIT").execute(&mut *conn).await?;
    Ok(())
}

/// Best effort: a failed rollback leaves nothing worse than the failure that
/// caused it, and the connection is dropped right after.
pub async fn rollback(conn: &mut SqliteConnection) {
    let _ = sqlx::query("ROLLBACK").execute(&mut *conn).await;
}

// ---------------------------------------------------------------------------
// Convenience: open a connection for the IPC layer and the gate
// ---------------------------------------------------------------------------

pub async fn open(user_id: &str, key_hex: &str) -> Result<SqliteConnection, SpendStoreError> {
    open_integration_keys_db(user_id, key_hex).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::FixedOffset;

    const TEST_KEY_HEX: &str = "0101010101010101010101010101010101010101010101010101010101010101";

    async fn with_conn<F, Fut>(f: F)
    where
        F: FnOnce(SqliteConnection) -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        let _lock = crate::test_support::ENV_MUTEX.lock().await;
        let saved_root = std::env::var("QR_DATA_ROOT").ok();
        let tempdir = tempfile::tempdir().expect("tempdir");
        std::env::set_var("QR_DATA_ROOT", tempdir.path());
        let conn = open("spend-user", TEST_KEY_HEX)
            .await
            .expect("open keys db");
        f(conn).await;
        if let Some(v) = saved_root {
            std::env::set_var("QR_DATA_ROOT", v);
        } else {
            std::env::remove_var("QR_DATA_ROOT");
        }
    }

    async fn outcome_of(conn: &mut SqliteConnection, id: &str) -> String {
        sqlx::query_scalar::<_, String>("SELECT outcome FROM provider_usage_ledger WHERE id = ?")
            .bind(id)
            .fetch_one(&mut *conn)
            .await
            .unwrap()
    }

    fn utc(y: i32, m: u32, d: u32, h: u32, min: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, m, d, h, min, 0).unwrap()
    }

    fn spec(
        metric: LimitMetric,
        period: LimitPeriod,
        scope: ScopeKind,
        id: Option<&str>,
    ) -> LimitSpec {
        LimitSpec {
            metric,
            period,
            scope_kind: scope,
            scope_id: id.map(str::to_owned),
            limit_value: 5_000_000,
            warn_at_pct: 80,
            approves_unattended: false,
        }
    }

    // -- time ---------------------------------------------------------------

    #[test]
    fn timestamps_are_fixed_width_and_sort_chronologically() {
        let a = ledger_timestamp(utc(2026, 10, 11, 18, 10));
        let b = ledger_timestamp(utc(2026, 10, 11, 18, 10) + Duration::microseconds(1));
        assert_eq!(a.len(), b.len());
        assert!(a < b);
        assert!(a.ends_with('Z'));
    }

    #[test]
    fn calendar_periods_follow_local_time() {
        // UTC-5. 2026-10-14 is a Wednesday. 03:00 UTC is still the 13th locally.
        let tz = FixedOffset::west_opt(5 * 3600).unwrap();
        let now = utc(2026, 10, 14, 3, 0);
        // Local time is Tue 2026-10-13 22:00.
        assert_eq!(
            period_start(LimitPeriod::Day, now, &tz),
            Some(utc(2026, 10, 13, 5, 0))
        );
        // ISO week starts Monday 2026-10-12 local midnight.
        assert_eq!(
            period_start(LimitPeriod::Week, now, &tz),
            Some(utc(2026, 10, 12, 5, 0))
        );
        assert_eq!(
            period_start(LimitPeriod::Month, now, &tz),
            Some(utc(2026, 10, 1, 5, 0))
        );
        assert_eq!(period_start(LimitPeriod::Run, now, &tz), None);
    }

    #[test]
    fn a_monday_is_its_own_week_start_and_a_sunday_belongs_to_the_prior_monday() {
        let tz = FixedOffset::east_opt(0).unwrap();
        // 2026-10-12 is a Monday.
        assert_eq!(
            period_start(LimitPeriod::Week, utc(2026, 10, 12, 0, 0), &tz),
            Some(utc(2026, 10, 12, 0, 0))
        );
        // 2026-10-18 is the Sunday of that week.
        assert_eq!(
            period_start(LimitPeriod::Week, utc(2026, 10, 18, 23, 59), &tz),
            Some(utc(2026, 10, 12, 0, 0))
        );
    }

    #[test]
    fn month_boundary_is_the_first_local_midnight() {
        let tz = FixedOffset::east_opt(9 * 3600).unwrap();
        // 2026-10-31 20:00 UTC is 2026-11-01 05:00 local.
        let now = utc(2026, 10, 31, 20, 0);
        assert_eq!(
            period_start(LimitPeriod::Month, now, &tz),
            Some(utc(2026, 10, 31, 15, 0))
        );
    }

    // -- limit validation -----------------------------------------------------

    #[test]
    fn limit_validation_rules() {
        let ok = spec(
            LimitMetric::CostUsd,
            LimitPeriod::Month,
            ScopeKind::All,
            None,
        );
        assert!(ok.validate().is_ok());
        let mut zero = ok.clone();
        zero.limit_value = 0;
        assert!(zero.validate().is_err());
        let mut pct = ok.clone();
        pct.warn_at_pct = 0;
        assert!(pct.validate().is_err());
        pct.warn_at_pct = 101;
        assert!(pct.validate().is_err());
        let all_with_id = spec(
            LimitMetric::CostUsd,
            LimitPeriod::Month,
            ScopeKind::All,
            Some("x"),
        );
        assert!(all_with_id.validate().is_err());
        let scoped_no_id = spec(
            LimitMetric::CostUsd,
            LimitPeriod::Month,
            ScopeKind::Provider,
            None,
        );
        assert!(scoped_no_id.validate().is_err());
        // Only a per-run cost limit can approve unattended spending.
        let mut approve = spec(LimitMetric::CostUsd, LimitPeriod::Run, ScopeKind::All, None);
        approve.approves_unattended = true;
        assert!(approve.validate().is_ok());
        approve.period = LimitPeriod::Month;
        assert!(approve.validate().is_err());
        approve.period = LimitPeriod::Run;
        approve.metric = LimitMetric::Requests;
        assert!(approve.validate().is_err());
    }

    // -- limits in the database ---------------------------------------------

    #[tokio::test]
    async fn limits_upsert_replace_remove_and_scope() {
        with_conn(|mut conn| async move {
            let now = utc(2026, 10, 11, 12, 0);
            let a = upsert_limit_conn(
                &mut conn,
                &spec(
                    LimitMetric::CostUsd,
                    LimitPeriod::Month,
                    ScopeKind::All,
                    None,
                ),
                now,
            )
            .await
            .unwrap();
            // Same key again updates in place.
            let mut changed = a.spec.clone();
            changed.limit_value = 9_000_000;
            let b = upsert_limit_conn(&mut conn, &changed, now).await.unwrap();
            assert_eq!(a.id, b.id);
            assert_eq!(b.spec.limit_value, 9_000_000);

            upsert_limit_conn(
                &mut conn,
                &spec(
                    LimitMetric::Requests,
                    LimitPeriod::Day,
                    ScopeKind::Provider,
                    Some("fixture-host-a"),
                ),
                now,
            )
            .await
            .unwrap();
            upsert_limit_conn(
                &mut conn,
                &spec(
                    LimitMetric::Requests,
                    LimitPeriod::Day,
                    ScopeKind::Provider,
                    Some("fixture-host-b"),
                ),
                now,
            )
            .await
            .unwrap();
            assert_eq!(list_limits_conn(&mut conn).await.unwrap().len(), 3);

            let for_a = applicable_limits_conn(&mut conn, "fixture-host-a", None, None)
                .await
                .unwrap();
            assert_eq!(for_a.len(), 2, "the all-providers limit and a's own");

            upsert_limit_conn(
                &mut conn,
                &spec(
                    LimitMetric::TokensOut,
                    LimitPeriod::Day,
                    ScopeKind::Persona,
                    Some("persona-1"),
                ),
                now,
            )
            .await
            .unwrap();
            upsert_limit_conn(
                &mut conn,
                &spec(
                    LimitMetric::TokensOut,
                    LimitPeriod::Run,
                    ScopeKind::Focus,
                    Some("focus-1"),
                ),
                now,
            )
            .await
            .unwrap();
            let scoped = applicable_limits_conn(
                &mut conn,
                "fixture-host-a",
                Some("persona-1"),
                Some("focus-1"),
            )
            .await
            .unwrap();
            assert_eq!(scoped.len(), 4);
            let other =
                applicable_limits_conn(&mut conn, "fixture-host-a", Some("persona-2"), None)
                    .await
                    .unwrap();
            assert_eq!(other.len(), 2);

            assert!(remove_limit_conn(
                &mut conn,
                LimitMetric::Requests,
                LimitPeriod::Day,
                ScopeKind::Provider,
                Some("fixture-host-b")
            )
            .await
            .unwrap());
            assert!(!remove_limit_conn(
                &mut conn,
                LimitMetric::Requests,
                LimitPeriod::Day,
                ScopeKind::Provider,
                Some("fixture-host-b")
            )
            .await
            .unwrap());
        })
        .await;
    }

    #[tokio::test]
    async fn the_database_refuses_an_unattended_flag_on_a_non_run_limit() {
        with_conn(|mut conn| async move {
            // Bypass the Rust validation to prove the schema CHECK holds.
            let bad = sqlx::query(
                "INSERT INTO provider_limits
                    (id, metric, period, scope_kind, scope_id, limit_value, warn_at_pct,
                     approves_unattended, created_at, updated_at)
                 VALUES ('l1', 'cost_usd', 'month', 'all', NULL, 1000, 80, 1, 't', 't')",
            )
            .execute(&mut conn)
            .await;
            assert!(
                bad.is_err(),
                "approves_unattended on a month limit must be refused"
            );
            let good = sqlx::query(
                "INSERT INTO provider_limits
                    (id, metric, period, scope_kind, scope_id, limit_value, warn_at_pct,
                     approves_unattended, created_at, updated_at)
                 VALUES ('l2', 'cost_usd', 'run', 'all', NULL, 1000, 80, 1, 't', 't')",
            )
            .execute(&mut conn)
            .await;
            assert!(good.is_ok());
        })
        .await;
    }

    // -- consent --------------------------------------------------------------

    #[tokio::test]
    async fn consent_is_append_only_and_revocation_clears_all_rows() {
        with_conn(|mut conn| async move {
            let t0 = utc(2026, 10, 11, 12, 0);
            let none = consent_standing_conn(&mut conn, "fixture-host-a")
                .await
                .unwrap();
            assert!(none.current.is_none() && !none.any_row);

            insert_consent_conn(&mut conn, "fixture-host-a", "hash-1", false, t0)
                .await
                .unwrap();
            insert_consent_conn(
                &mut conn,
                "fixture-host-a",
                "hash-2",
                true,
                t0 + Duration::seconds(5),
            )
            .await
            .unwrap();
            let s = consent_standing_conn(&mut conn, "fixture-host-a")
                .await
                .unwrap();
            let cur = s.current.unwrap();
            assert_eq!(cur.attributes_hash, "hash-2");
            assert!(cur.no_cap_confirmed);

            // Another provider is unaffected by a revoke.
            insert_consent_conn(&mut conn, "fixture-host-b", "hash-b", false, t0)
                .await
                .unwrap();
            assert_eq!(
                revoke_consent_conn(&mut conn, "fixture-host-a", t0 + Duration::seconds(9))
                    .await
                    .unwrap(),
                2
            );
            let after = consent_standing_conn(&mut conn, "fixture-host-a")
                .await
                .unwrap();
            assert!(
                after.current.is_none(),
                "revoking must not fall back to the older row"
            );
            assert!(after.any_row, "history is kept");
            let b = consent_standing_conn(&mut conn, "fixture-host-b")
                .await
                .unwrap();
            assert!(b.current.is_some());

            // Consenting again after a revoke starts a new current row.
            insert_consent_conn(
                &mut conn,
                "fixture-host-a",
                "hash-3",
                false,
                t0 + Duration::seconds(20),
            )
            .await
            .unwrap();
            let again = consent_standing_conn(&mut conn, "fixture-host-a")
                .await
                .unwrap();
            assert_eq!(again.current.unwrap().attributes_hash, "hash-3");
        })
        .await;
    }

    // -- ledger ---------------------------------------------------------------

    fn reservation(run: &str) -> NewReservation<'_> {
        NewReservation {
            provider_id: "fixture-host-a",
            model_id: "fixture-model-a",
            persona_id: Some("persona-1"),
            focus_id: Some("focus-1"),
            run_id: Some(run),
            tokens_in: 100,
            tokens_out: 4096,
            cost_micros: 8_000,
            price_source: "curated",
        }
    }

    #[tokio::test]
    async fn a_reservation_counts_at_its_worst_case_until_settled() {
        with_conn(|mut conn| async move {
            let now = utc(2026, 10, 11, 12, 0);
            let id = insert_reservation_conn(&mut conn, &reservation("run-1"), now)
                .await
                .unwrap();
            let held = usage_totals_conn(&mut conn, &UsageFilter { run_id: Some("run-1"), ..Default::default() })
                .await
                .unwrap();
            assert_eq!(held, UsageTotals { cost_micros: 8_000, tokens_in: 100, tokens_out: 4096, requests: 1 });

            assert!(settle_conn(
                &mut conn,
                &id,
                &Settlement {
                    tokens_in: 90,
                    tokens_out: 300,
                    reasoning_tokens: Some(120),
                    cost_micros: 650,
                    outcome: LedgerOutcome::Ok,
                    usage_source: UsageSource::Reported,
                }
            )
            .await
            .unwrap());
            let settled = usage_totals_conn(&mut conn, &UsageFilter { run_id: Some("run-1"), ..Default::default() })
                .await
                .unwrap();
            assert_eq!(settled, UsageTotals { cost_micros: 650, tokens_in: 90, tokens_out: 300, requests: 1 });

            // Settling again changes nothing.
            assert!(!settle_conn(
                &mut conn,
                &id,
                &Settlement {
                    tokens_in: 0,
                    tokens_out: 0,
                    reasoning_tokens: None,
                    cost_micros: 0,
                    outcome: LedgerOutcome::Failed,
                    usage_source: UsageSource::None,
                }
            )
            .await
            .unwrap());
            let row = sqlx::query("SELECT reasoning_tokens, outcome, usage_source, reserved_micros FROM provider_usage_ledger WHERE id = ?")
                .bind(&id)
                .fetch_one(&mut conn)
                .await
                .unwrap();
            assert_eq!(row.get::<Option<i64>, _>("reasoning_tokens"), Some(120));
            assert_eq!(row.get::<String, _>("outcome"), "ok");
            assert_eq!(row.get::<String, _>("usage_source"), "reported");
            assert_eq!(row.get::<Option<i64>, _>("reserved_micros"), Some(8_000));
        })
        .await;
    }

    #[tokio::test]
    async fn interrupted_keeps_the_reserved_amounts_and_release_removes_the_row() {
        with_conn(|mut conn| async move {
            let now = utc(2026, 10, 11, 12, 0);
            let a = insert_reservation_conn(&mut conn, &reservation("run-1"), now)
                .await
                .unwrap();
            let b = insert_reservation_conn(&mut conn, &reservation("run-1"), now)
                .await
                .unwrap();
            assert!(mark_interrupted_conn(&mut conn, &a).await.unwrap());
            assert!(release_reservation_conn(&mut conn, &b).await.unwrap());
            // An interrupted row is not pending any more, so it is not released.
            assert!(!release_reservation_conn(&mut conn, &a).await.unwrap());
            let t = usage_totals_conn(
                &mut conn,
                &UsageFilter {
                    run_id: Some("run-1"),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
            assert_eq!(t.cost_micros, 8_000);
            assert_eq!(t.requests, 1);
            let row =
                sqlx::query("SELECT outcome, usage_source FROM provider_usage_ledger WHERE id = ?")
                    .bind(&a)
                    .fetch_one(&mut conn)
                    .await
                    .unwrap();
            assert_eq!(row.get::<String, _>("outcome"), "interrupted");
            assert_eq!(row.get::<String, _>("usage_source"), "none");
        })
        .await;
    }

    #[tokio::test]
    async fn startup_recovery_converts_only_stale_pending_rows() {
        with_conn(|mut conn| async move {
            let now = utc(2026, 10, 11, 12, 0);
            let stale = insert_reservation_conn(
                &mut conn,
                &reservation("run-old"),
                now - Duration::minutes(30),
            )
            .await
            .unwrap();
            let fresh = insert_reservation_conn(
                &mut conn,
                &reservation("run-new"),
                now - Duration::minutes(1),
            )
            .await
            .unwrap();
            let settled = insert_reservation_conn(
                &mut conn,
                &reservation("run-done"),
                now - Duration::minutes(30),
            )
            .await
            .unwrap();
            settle_conn(
                &mut conn,
                &settled,
                &Settlement {
                    tokens_in: 1,
                    tokens_out: 1,
                    reasoning_tokens: None,
                    cost_micros: 1,
                    outcome: LedgerOutcome::Ok,
                    usage_source: UsageSource::Reported,
                },
            )
            .await
            .unwrap();

            assert_eq!(recover_interrupted_conn(&mut conn, now).await.unwrap(), 1);
            assert_eq!(outcome_of(&mut conn, &stale).await, "interrupted");
            assert_eq!(outcome_of(&mut conn, &fresh).await, "pending");
            assert_eq!(outcome_of(&mut conn, &settled).await, "ok");
            // The stale row still counts at its reserved worst case.
            let t = usage_totals_conn(
                &mut conn,
                &UsageFilter {
                    run_id: Some("run-old"),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
            assert_eq!(t.cost_micros, 8_000);
            // Running it again converts nothing.
            assert_eq!(recover_interrupted_conn(&mut conn, now).await.unwrap(), 0);
        })
        .await;
    }

    /// The login step opens the user's store itself and recovers, without ever
    /// failing the login.
    #[tokio::test]
    async fn login_recovery_converts_abandoned_reservations() {
        with_conn(|mut conn| async move {
            let long_ago = Utc::now() - Duration::hours(2);
            let id = insert_reservation_conn(&mut conn, &reservation("run-crash"), long_ago)
                .await
                .unwrap();
            drop(conn);
            assert_eq!(
                recover_interrupted_spend("spend-user", TEST_KEY_HEX).await,
                1
            );
            let mut conn = open("spend-user", TEST_KEY_HEX).await.unwrap();
            assert_eq!(outcome_of(&mut conn, &id).await, "interrupted");
            // Recovery on an unreadable store is a no-op, not a failure.
            assert_eq!(
                recover_interrupted_spend("spend-user", "not-the-key").await,
                0
            );
        })
        .await;
    }

    #[tokio::test]
    async fn usage_filters_by_provider_persona_focus_run_and_window() {
        with_conn(|mut conn| async move {
            let base = utc(2026, 10, 11, 12, 0);
            for (i, (prov, pers, run, at)) in [
                ("fixture-host-a", "persona-1", "run-1", base),
                (
                    "fixture-host-a",
                    "persona-2",
                    "run-2",
                    base + Duration::days(1),
                ),
                (
                    "fixture-host-b",
                    "persona-1",
                    "run-3",
                    base + Duration::days(2),
                ),
            ]
            .into_iter()
            .enumerate()
            {
                let r = NewReservation {
                    provider_id: prov,
                    model_id: "fixture-model-a",
                    persona_id: Some(pers),
                    focus_id: Some("focus-1"),
                    run_id: Some(run),
                    tokens_in: 10,
                    tokens_out: 20,
                    cost_micros: 100 * (i as i64 + 1),
                    price_source: "curated",
                };
                insert_reservation_conn(&mut conn, &r, at).await.unwrap();
            }
            let all = usage_totals_conn(&mut conn, &UsageFilter::default())
                .await
                .unwrap();
            assert_eq!(all.cost_micros, 600);
            assert_eq!(all.requests, 3);
            let prov_a = usage_totals_conn(
                &mut conn,
                &UsageFilter {
                    provider_id: Some("fixture-host-a"),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
            assert_eq!(prov_a.cost_micros, 300);
            let pers_1 = usage_totals_conn(
                &mut conn,
                &UsageFilter {
                    persona_id: Some("persona-1"),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
            assert_eq!(pers_1.cost_micros, 400);
            let since = ledger_timestamp(base + Duration::days(1));
            let window = usage_totals_conn(
                &mut conn,
                &UsageFilter {
                    since: Some(&since),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
            assert_eq!(window.cost_micros, 500, "the window start is inclusive");
            let by = usage_by_provider_conn(&mut conn, None).await.unwrap();
            assert_eq!(by.len(), 2);
            assert_eq!(by[0].0, "fixture-host-a");
            assert_eq!(by[0].1.cost_micros, 300);
        })
        .await;
    }

    // -- snapshot, overrides, calibration --------------------------------------

    #[tokio::test]
    async fn rate_snapshot_replaces_and_ignores_silence() {
        with_conn(|mut conn| async move {
            let t = utc(2026, 10, 11, 12, 0);
            let info = RateLimitInfo::from_pairs(&[
                ("x-ratelimit-remaining-requests", "41"),
                ("x-ratelimit-limit-requests", "50"),
                ("x-ratelimit-remaining-tokens", "9000"),
                ("x-ratelimit-reset-tokens", "6s"),
            ]);
            assert!(
                upsert_rate_snapshot_conn(&mut conn, "fixture-host-a", &info, t)
                    .await
                    .unwrap()
            );
            let s = get_rate_snapshot_conn(&mut conn, "fixture-host-a")
                .await
                .unwrap()
                .unwrap();
            assert_eq!(s.requests_remaining, Some(41));
            assert_eq!(s.requests_limit, Some(50));
            assert_eq!(s.tokens_remaining, Some(9000));
            assert_eq!(s.tokens_reset.as_deref(), Some("6s"));

            // A response with no rate-limit headers leaves the snapshot alone.
            assert!(!upsert_rate_snapshot_conn(
                &mut conn,
                "fixture-host-a",
                &RateLimitInfo::default(),
                t + Duration::seconds(5)
            )
            .await
            .unwrap());
            let still = get_rate_snapshot_conn(&mut conn, "fixture-host-a")
                .await
                .unwrap()
                .unwrap();
            assert_eq!(still, s);

            let newer = RateLimitInfo::from_pairs(&[("x-ratelimit-remaining-requests", "40")]);
            upsert_rate_snapshot_conn(
                &mut conn,
                "fixture-host-a",
                &newer,
                t + Duration::seconds(9),
            )
            .await
            .unwrap();
            let s2 = get_rate_snapshot_conn(&mut conn, "fixture-host-a")
                .await
                .unwrap()
                .unwrap();
            assert_eq!(s2.requests_remaining, Some(40));
            assert_eq!(
                s2.tokens_remaining, None,
                "a new observation replaces the old one whole"
            );
        })
        .await;
    }

    #[tokio::test]
    async fn overrides_and_calibration_read_back() {
        with_conn(|mut conn| async move {
            assert!(get_price_override_raw_conn(&mut conn, "fixture-host-a", "fixture-model-a")
                .await
                .unwrap()
                .is_none());
            sqlx::query("INSERT INTO price_overrides (provider_id, model_id, pricing, updated_at) VALUES ('fixture-host-a', 'fixture-model-a', '{\"input_per_mtok\":1,\"output_per_mtok\":2}', 't')")
                .execute(&mut conn)
                .await
                .unwrap();
            let raw = get_price_override_raw_conn(&mut conn, "fixture-host-a", "fixture-model-a")
                .await
                .unwrap()
                .unwrap();
            assert!(raw.contains("input_per_mtok"));

            assert!(get_calibration_conn(&mut conn, "fixture-host-a", "fixture-model-a").await.unwrap().is_none());
            sqlx::query("INSERT INTO calibration (provider_id, model_id, input_factor, output_factor, sample_count, updated_at) VALUES ('fixture-host-a', 'fixture-model-a', 1.25, 1.5, 7, 't')")
                .execute(&mut conn)
                .await
                .unwrap();
            let c = get_calibration_conn(&mut conn, "fixture-host-a", "fixture-model-a")
                .await
                .unwrap()
                .unwrap();
            assert_eq!(c.input_factor, "1.25");
            assert_eq!(c.output_factor, "1.5");
            assert_eq!(c.sample_count, 7);
            assert_eq!(c.output_sample_count, 0);
        })
        .await;
    }

    #[tokio::test]
    async fn calibration_writer_round_trips_and_replaces() {
        with_conn(|mut conn| async move {
            let now = Utc::now();
            upsert_calibration_conn(
                &mut conn,
                &CalibrationWrite {
                    provider_id: "p",
                    model_id: "m",
                    input_factor: "1.234",
                    output_factor: "0.5",
                    sample_count: 3,
                    output_sample_count: 2,
                },
                now,
            )
            .await
            .unwrap();
            let c = get_calibration_conn(&mut conn, "p", "m")
                .await
                .unwrap()
                .unwrap();
            assert_eq!(c.input_factor, "1.234");
            assert_eq!(c.output_factor, "0.5");
            assert_eq!((c.sample_count, c.output_sample_count), (3, 2));
            upsert_calibration_conn(
                &mut conn,
                &CalibrationWrite {
                    provider_id: "p",
                    model_id: "m",
                    input_factor: "2",
                    output_factor: "1",
                    sample_count: 4,
                    output_sample_count: 2,
                },
                now,
            )
            .await
            .unwrap();
            let all = list_calibrations_conn(&mut conn, "p").await.unwrap();
            assert_eq!(all.len(), 1);
            assert_eq!(all[0].1.sample_count, 4);
            // A factor of zero or below is refused by the schema.
            assert!(upsert_calibration_conn(
                &mut conn,
                &CalibrationWrite {
                    provider_id: "p",
                    model_id: "m",
                    input_factor: "0",
                    output_factor: "1",
                    sample_count: 1,
                    output_sample_count: 0,
                },
                now,
            )
            .await
            .is_err());
        })
        .await;
    }

    #[tokio::test]
    async fn price_override_writer_round_trips_lists_and_deletes() {
        with_conn(|mut conn| async move {
            let now = Utc::now();
            upsert_price_override_conn(
                &mut conn,
                "p",
                "m",
                r#"{"input_per_mtok":1,"output_per_mtok":2}"#,
                now,
            )
            .await
            .unwrap();
            upsert_price_override_conn(
                &mut conn,
                "p",
                "m",
                r#"{"input_per_mtok":3,"output_per_mtok":4}"#,
                now,
            )
            .await
            .unwrap();
            upsert_price_override_conn(
                &mut conn,
                "other",
                "m",
                r#"{"input_per_mtok":9,"output_per_mtok":9}"#,
                now,
            )
            .await
            .unwrap();
            let listed = list_price_overrides_conn(&mut conn, "p").await.unwrap();
            assert_eq!(listed.len(), 1);
            assert!(listed[0].1.contains(":3,"));
            assert!(delete_price_override_conn(&mut conn, "p", "m")
                .await
                .unwrap());
            assert!(!delete_price_override_conn(&mut conn, "p", "m")
                .await
                .unwrap());
            assert!(get_price_override_raw_conn(&mut conn, "p", "m")
                .await
                .unwrap()
                .is_none());
            assert!(get_price_override_raw_conn(&mut conn, "other", "m")
                .await
                .unwrap()
                .is_some());
        })
        .await;
    }
}
