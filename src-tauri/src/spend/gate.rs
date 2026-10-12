// src-tauri/src/spend/gate.rs
//
// The spend pre-flight and the metered call (items.id=710; decisions.id=875
// point 4; decisions.id=872 point 6; spec Part 8e). Runs in the executor's
// hosted branch, AFTER the call context is built and BEFORE any network call.
// Nothing leaves the machine unless every check here passes.
//
// ORDER (each failure is a `SpendBlock`: a stable code plus structured
// parameters, never prose; the stop/park UX is items.id=696):
//   1. consent       none/revoked -> consent_required; attributes changed since
//                    consent -> reconsent_required
//   2. price         unknown (uncurated model, no override) -> price_unknown.
//                    An unknown price cannot be capped, so it stops and asks.
//   3. cap rule      a call that can cost money needs an applicable day, week
//                    or month cost limit, or an explicit no-cap confirmation
//                    on the consent -> cap_required
//   4. unattended    a run with no user present (or whose attendance is not
//                    known) spends only within an approved per-run budget:
//                    at least one applicable per-run cost limit the user
//                    marked approves_unattended, else budget_approval_required
//                    (the run parks). A call that costs nothing needs none.
//   5. limits        every applicable limit (scope all/provider/persona/focus;
//                    cost/tokens in/tokens out/requests; run/day/week/month):
//                    spent + this call's projected use > limit -> limit_reached.
//                    At or above the warn threshold it still proceeds and is
//                    reported as a notice. The most restrictive limit governs
//                    because all of them must pass.
//   6. reserve       a ledger row holding the worst-case amounts is written in
//                    the SAME `BEGIN IMMEDIATE` transaction as the reads above,
//                    so two concurrent calls cannot both slip under a cap.
//
// SETTLEMENT (after the call): the reservation is overwritten with the real
// usage. See `settlement_for` for every outcome. A call that burned tokens and
// then failed to yield an answer is charged for them. A call that may have
// reached the provider but returned no usage is charged at its reserved worst
// case. A provider error status is a request with no cost. A call that never
// left the machine is released.
//
// MONEY is integer micro-USD (spend::money); no floats in this file. The
// scan test at the bottom keeps it that way.

use chrono::{DateTime, TimeZone, Utc};
use serde_json::{json, Map, Value};
use sqlx::SqliteConnection;

use crate::persistence::provider_spend_store::{
    self as store, ConsentStanding, LedgerOutcome, LimitMetric, LimitPeriod, LimitRow,
    NewReservation, ScopeKind, Settlement, UsageFilter, UsageSource, UsageTotals,
};
use crate::persistence::provider_store::{Provider, ProviderModel};
use crate::providers::errors::ProviderError;
use crate::providers::hosted_call::CallContext;
use crate::providers::hosted_client::{
    effective_output_limit, missing_limit_error, HostedAttempt, HostedClient, HostedOutcome,
    HostedRequest,
};
use crate::spend::codes::Code;
use crate::spend::disclosure::attributes_hash;
use crate::spend::estimate::{
    actual_micros, input_tokens_estimate, worst_case, Estimate, NEUTRAL_PERMILLE,
};
use crate::spend::money::{permille_ceil, permille_ceil_text};
use crate::spend::price::{resolve_price, PriceQuote};

// ---------------------------------------------------------------------------
// Attendance
// ---------------------------------------------------------------------------

/// Whether a user is present for a run. Only `Attended` may spend beyond an
/// approved per-run budget. The default everywhere is `Unattended`: a run
/// whose attendance cannot be determined parks rather than spends
/// (no-autonomous-action tenet; review ruling 1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RunAttendance {
    Attended,
    #[default]
    Unattended,
}

// ---------------------------------------------------------------------------
// Blocks and notices
// ---------------------------------------------------------------------------

/// A call stopped (or a run parked) before any network traffic. A code plus
/// structured parameters; the user-facing copy renders in the frontend.
#[derive(Debug, Clone, PartialEq)]
pub struct SpendBlock {
    pub code: Code,
    pub params: Map<String, Value>,
}

impl SpendBlock {
    pub fn new(code: Code) -> Self {
        Self {
            code,
            params: Map::new(),
        }
    }

    pub fn with(mut self, key: &str, value: impl Into<Value>) -> Self {
        self.params.insert(key.to_owned(), value.into());
        self
    }

    pub fn code_str(&self) -> &'static str {
        self.code.as_str()
    }
}

/// A limit the call will push to or past its warning threshold (it still
/// proceeds), reported for the usage summary and future events.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LimitNotice {
    pub limit_id: String,
    pub metric: LimitMetric,
    pub period: LimitPeriod,
    pub scope_kind: ScopeKind,
    pub scope_id: Option<String>,
    pub spent: i64,
    pub limit: i64,
    pub projected_add: i64,
}

// ---------------------------------------------------------------------------
// Pure decision
// ---------------------------------------------------------------------------

/// A limit with what has already been used inside its scope and window.
#[derive(Debug, Clone)]
pub struct LimitFact {
    pub row: LimitRow,
    pub spent: UsageTotals,
}

pub struct Facts<'a> {
    pub standing: &'a ConsentStanding,
    pub current_hash: &'a str,
    pub quote: &'a PriceQuote,
    pub estimate: Option<&'a Estimate>,
    pub limits: &'a [LimitFact],
    pub attendance: RunAttendance,
}

fn spent_for(metric: LimitMetric, t: &UsageTotals) -> i64 {
    match metric {
        LimitMetric::CostUsd => t.cost_micros,
        LimitMetric::TokensIn => t.tokens_in,
        LimitMetric::TokensOut => t.tokens_out,
        LimitMetric::Requests => t.requests,
    }
}

fn add_for(metric: LimitMetric, e: &Estimate) -> i64 {
    let clamp = |v: u64| i64::try_from(v).unwrap_or(i64::MAX);
    match metric {
        LimitMetric::CostUsd => e.worst_case_micros,
        LimitMetric::TokensIn => clamp(e.input_tokens),
        LimitMetric::TokensOut => clamp(e.output_limit_tokens),
        LimitMetric::Requests => 1,
    }
}

/// Decide whether a call may proceed. No I/O.
pub fn evaluate(f: &Facts<'_>) -> Result<Vec<LimitNotice>, SpendBlock> {
    // 1. Consent.
    let consent = match &f.standing.current {
        None => {
            return Err(SpendBlock::new(Code::BlockConsentRequired).with(
                "state",
                if f.standing.any_row {
                    "revoked"
                } else {
                    "none"
                },
            ))
        }
        Some(c) if c.attributes_hash != f.current_hash => {
            return Err(SpendBlock::new(Code::BlockReconsentRequired))
        }
        Some(c) => c,
    };

    // 2. Price.
    let (PriceQuote::Known(_), Some(estimate)) = (f.quote, f.estimate) else {
        return Err(SpendBlock::new(Code::BlockPriceUnknown));
    };
    let worst = estimate.worst_case_micros;
    let can_cost = worst > 0;

    // 3. Cap rule.
    if can_cost {
        let has_cap = f.limits.iter().any(|l| {
            l.row.spec.metric == LimitMetric::CostUsd
                && matches!(
                    l.row.spec.period,
                    LimitPeriod::Day | LimitPeriod::Week | LimitPeriod::Month
                )
        });
        if !has_cap && !consent.no_cap_confirmed {
            return Err(SpendBlock::new(Code::BlockCapRequired).with("estimate_micros", worst));
        }
    }

    // 4. Unattended runs need an approved per-run budget.
    if can_cost && f.attendance != RunAttendance::Attended {
        let approved = f.limits.iter().any(|l| {
            l.row.spec.approves_unattended
                && l.row.spec.metric == LimitMetric::CostUsd
                && l.row.spec.period == LimitPeriod::Run
        });
        if !approved {
            return Err(SpendBlock::new(Code::BlockBudgetApprovalRequired)
                .with("estimate_micros", worst)
                .with("missing_approval", "per_run_cost_limit"));
        }
    }

    // 5. Every applicable limit.
    let mut notices = Vec::new();
    for fact in f.limits {
        let spec = &fact.row.spec;
        let spent = spent_for(spec.metric, &fact.spent);
        let add = add_for(spec.metric, estimate);
        // An overflowing projection is over any limit.
        let projected = spent.checked_add(add);
        if projected.is_none_or(|p| p > spec.limit_value) {
            return Err(SpendBlock::new(Code::BlockLimitReached)
                .with("limit_id", fact.row.id.clone())
                .with("metric", spec.metric.as_str())
                .with("period", spec.period.as_str())
                .with("scope_kind", spec.scope_kind.as_str())
                .with("scope_id", spec.scope_id.clone())
                .with("spent", spent)
                .with("limit", spec.limit_value)
                .with("projected_add", add)
                .with("approves_unattended", spec.approves_unattended));
        }
        let projected = projected.unwrap_or(i64::MAX);
        let at_warn = u128::try_from(projected).unwrap_or(0) * 100
            >= u128::try_from(spec.limit_value).unwrap_or(0) * u128::from(spec.warn_at_pct);
        if at_warn {
            notices.push(LimitNotice {
                limit_id: fact.row.id.clone(),
                metric: spec.metric,
                period: spec.period,
                scope_kind: spec.scope_kind,
                scope_id: spec.scope_id.clone(),
                spent,
                limit: spec.limit_value,
                projected_add: add,
            });
        }
    }
    Ok(notices)
}

// ---------------------------------------------------------------------------
// The call, as the gate sees it
// ---------------------------------------------------------------------------

/// Everything the gate needs to know about one hosted call beyond the call
/// context itself.
pub struct SpendCall<'a> {
    pub user_id: &'a str,
    pub key_hex: &'a str,
    pub provider: &'a Provider,
    pub model: &'a ProviderModel,
    /// Every model row of the provider (the attributes hash spans them all).
    pub all_models: &'a [ProviderModel],
    /// Raw `pricing` text per model (exact price parsing needs the text).
    pub pricing_texts: &'a [(String, Option<String>)],
    pub persona_id: Option<&'a str>,
    pub focus_id: Option<&'a str>,
    pub run_id: Option<&'a str>,
    pub attendance: RunAttendance,
}

/// A held reservation plus the figures settlement needs.
#[derive(Debug, Clone)]
pub struct Reserved {
    pub reservation_id: String,
    pub quote: PriceQuote,
    pub input_tokens: u64,
    pub output_limit: u64,
    pub worst_case_micros: i64,
    pub notices: Vec<LimitNotice>,
}

fn price_source_of(quote: &PriceQuote) -> &'static str {
    match quote {
        PriceQuote::Known(p) => p.source.as_str(),
        PriceQuote::Unknown => "unknown",
    }
}

fn model_pricing_text<'a>(call: &'a SpendCall<'_>) -> Option<&'a str> {
    call.pricing_texts
        .iter()
        .find(|(id, _)| *id == call.model.model_id)
        .and_then(|(_, t)| t.as_deref())
}

/// The scope filter for one limit: which ledger rows count against it.
fn filter_for<'a>(
    row: &'a LimitRow,
    run_id: Option<&'a str>,
    since: Option<&'a str>,
) -> UsageFilter<'a> {
    let spec = &row.spec;
    let mut f = UsageFilter {
        since,
        ..Default::default()
    };
    match spec.scope_kind {
        ScopeKind::All => {}
        ScopeKind::Provider => f.provider_id = spec.scope_id.as_deref(),
        ScopeKind::Persona => f.persona_id = spec.scope_id.as_deref(),
        ScopeKind::Focus => f.focus_id = spec.scope_id.as_deref(),
    }
    // A per-run limit counts this run only. With no run id the filter stays
    // unrestricted, which over-counts: the safe direction.
    if spec.period == LimitPeriod::Run {
        f.run_id = run_id;
    }
    f
}

async fn gather_and_decide<Tz: TimeZone>(
    conn: &mut SqliteConnection,
    call: &SpendCall<'_>,
    prompt_bytes: u64,
    output_limit: u64,
    tz: &Tz,
    now: DateTime<Utc>,
) -> Result<(Reserved, Vec<LimitNotice>), SpendBlock> {
    let ledger_down = |e: store::SpendStoreError| {
        log::warn!("spend pre-flight: ledger unavailable: {e}");
        SpendBlock::new(Code::BlockLedgerUnavailable)
    };
    let provider_id = call.provider.id.as_str();
    let model_id = call.model.model_id.as_str();

    let current_hash = attributes_hash(call.provider, call.all_models, call.pricing_texts);
    let standing = store::consent_standing_conn(conn, provider_id)
        .await
        .map_err(ledger_down)?;

    let override_raw = store::get_price_override_raw_conn(conn, provider_id, model_id)
        .await
        .map_err(ledger_down)?;
    let quote = resolve_price(override_raw.as_deref(), model_pricing_text(call));

    let calibration = store::get_calibration_conn(conn, provider_id, model_id)
        .await
        .map_err(ledger_down)?;
    let input_cal = calibration
        .as_ref()
        .and_then(|c| permille_ceil_text(&c.input_factor))
        .unwrap_or(NEUTRAL_PERMILLE);
    // A row multiplier that is unusable is a data fault: take the neutral
    // factor rather than fail the call (the worst case below stays bounded by
    // the output limit regardless).
    let token_factor = permille_ceil(call.model.token_factor).unwrap_or(NEUTRAL_PERMILLE);
    let input_tokens = input_tokens_estimate(prompt_bytes, token_factor, input_cal);

    let estimate = match &quote {
        PriceQuote::Known(price) => Some(worst_case(price, input_tokens, output_limit)),
        PriceQuote::Unknown => None,
    };

    let limit_rows =
        store::applicable_limits_conn(conn, provider_id, call.persona_id, call.focus_id)
            .await
            .map_err(ledger_down)?;
    let mut facts = Vec::with_capacity(limit_rows.len());
    for row in limit_rows {
        let since_ts = store::period_start(row.spec.period, now, tz).map(store::ledger_timestamp);
        let filter = filter_for(&row, call.run_id, since_ts.as_deref());
        let spent = store::usage_totals_conn(conn, &filter)
            .await
            .map_err(ledger_down)?;
        facts.push(LimitFact { row, spent });
    }

    let notices = evaluate(&Facts {
        standing: &standing,
        current_hash: &current_hash,
        quote: &quote,
        estimate: estimate.as_ref(),
        limits: &facts,
        attendance: call.attendance,
    })?;

    // Cleared: hold the worst case. `estimate` is Some here (a Known price).
    let estimate = estimate.ok_or_else(|| SpendBlock::new(Code::BlockPriceUnknown))?;
    let id = store::insert_reservation_conn(
        conn,
        &NewReservation {
            provider_id,
            model_id,
            persona_id: call.persona_id,
            focus_id: call.focus_id,
            run_id: call.run_id,
            tokens_in: i64::try_from(input_tokens).unwrap_or(i64::MAX),
            tokens_out: i64::try_from(output_limit).unwrap_or(i64::MAX),
            cost_micros: estimate.worst_case_micros,
            price_source: price_source_of(&quote),
        },
        now,
    )
    .await
    .map_err(ledger_down)?;

    Ok((
        Reserved {
            reservation_id: id,
            quote,
            input_tokens,
            output_limit,
            worst_case_micros: estimate.worst_case_micros,
            notices: notices.clone(),
        },
        notices,
    ))
}

/// The pre-flight: decide and, when cleared, reserve - atomically. `tz` is the
/// user's local time zone (`chrono::Local` in production) because calendar
/// periods follow it.
pub async fn preflight<Tz: TimeZone>(
    conn: &mut SqliteConnection,
    call: &SpendCall<'_>,
    prompt_bytes: u64,
    output_limit: u64,
    tz: &Tz,
    now: DateTime<Utc>,
) -> Result<Reserved, SpendBlock> {
    if let Err(e) = store::begin_immediate(conn).await {
        log::warn!("spend pre-flight: could not start a transaction: {e}");
        return Err(SpendBlock::new(Code::BlockLedgerUnavailable));
    }
    match gather_and_decide(conn, call, prompt_bytes, output_limit, tz, now).await {
        Ok((reserved, _)) => match store::commit(conn).await {
            Ok(()) => Ok(reserved),
            Err(e) => {
                log::warn!("spend pre-flight: commit failed: {e}");
                store::rollback(conn).await;
                Err(SpendBlock::new(Code::BlockLedgerUnavailable))
            }
        },
        Err(block) => {
            store::rollback(conn).await;
            Err(block)
        }
    }
}

// ---------------------------------------------------------------------------
// Settlement
// ---------------------------------------------------------------------------

/// What to do with the reservation once the call has finished.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettleAction {
    /// Overwrite the reservation with these figures.
    Settle(Settlement),
    /// May have reached the provider with no usage returned: keep the
    /// reserved amounts and mark the row interrupted.
    Interrupt,
    /// The call never left the machine: drop the reservation.
    Release,
}

fn clamp_i64(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

fn cost_of(quote: &PriceQuote, prompt: u64, completion: u64) -> i64 {
    match quote {
        PriceQuote::Known(p) => actual_micros(p, prompt, completion),
        PriceQuote::Unknown => 0,
    }
}

/// Decide the settlement for a finished attempt. Pure.
pub fn settlement_for(reserved: &Reserved, attempt: &HostedAttempt) -> SettleAction {
    // Tokens the provider reported, whether or not an answer came back.
    if let Some(u) = attempt.usage {
        let prompt = u64::from(u.prompt_tokens);
        let completion = u64::from(u.completion_tokens);
        let outcome = match &attempt.result {
            Ok(o) if o.truncated => LedgerOutcome::Truncated,
            Ok(_) => LedgerOutcome::Ok,
            Err(ProviderError::OutputBudgetExhausted { .. }) => LedgerOutcome::BudgetExhausted,
            Err(_) => LedgerOutcome::Failed,
        };
        return SettleAction::Settle(Settlement {
            tokens_in: clamp_i64(prompt),
            tokens_out: clamp_i64(completion),
            reasoning_tokens: u.reasoning_tokens.map(i64::from),
            cost_micros: cost_of(&reserved.quote, prompt, completion),
            outcome,
            usage_source: UsageSource::Reported,
        });
    }

    match &attempt.result {
        // A successful answer whose response carried no usage: charge QR's
        // own estimate and say so.
        Ok(o) => {
            let prompt = reserved.input_tokens;
            let completion = (o.response.content.len() as u64).div_ceil(4);
            SettleAction::Settle(Settlement {
                tokens_in: clamp_i64(prompt),
                tokens_out: clamp_i64(completion),
                reasoning_tokens: None,
                cost_micros: cost_of(&reserved.quote, prompt, completion),
                outcome: if o.truncated {
                    LedgerOutcome::Truncated
                } else {
                    LedgerOutcome::Ok
                },
                usage_source: UsageSource::Estimated,
            })
        }
        Err(_) => {
            if !attempt.reached_provider {
                return SettleAction::Release;
            }
            match attempt.http_status {
                // The provider answered with an error status: a request, no
                // generation, no cost.
                Some(code) if !(200..300).contains(&code) => SettleAction::Settle(Settlement {
                    tokens_in: 0,
                    tokens_out: 0,
                    reasoning_tokens: None,
                    cost_micros: 0,
                    outcome: LedgerOutcome::Failed,
                    usage_source: UsageSource::None,
                }),
                // No response, or a 2xx whose body gave no usage: unknown
                // outcome, charged at the reserved worst case.
                _ => SettleAction::Interrupt,
            }
        }
    }
}

/// Apply the settlement and record the rate-limit observation. Best effort:
/// a failure here is logged and leaves the reservation counted at its worst
/// case (login recovery converts it later), never failing the user's call.
pub async fn finish(
    conn: &mut SqliteConnection,
    reserved: &Reserved,
    provider_id: &str,
    attempt: &HostedAttempt,
    now: DateTime<Utc>,
) {
    let id = reserved.reservation_id.as_str();
    let applied = match settlement_for(reserved, attempt) {
        SettleAction::Settle(s) => store::settle_conn(conn, id, &s).await,
        SettleAction::Interrupt => store::mark_interrupted_conn(conn, id).await,
        SettleAction::Release => store::release_reservation_conn(conn, id).await,
    };
    if let Err(e) = applied {
        log::warn!("spend settlement failed (the reservation stays counted): {e}");
    }
    if let Err(e) =
        store::upsert_rate_snapshot_conn(conn, provider_id, &attempt.rate_limit, now).await
    {
        log::warn!("spend: rate-limit snapshot not recorded: {e}");
    }
}

// ---------------------------------------------------------------------------
// The metered call
// ---------------------------------------------------------------------------

/// Why a metered call did not produce an answer.
#[derive(Debug)]
pub enum SpendCallError {
    /// Stopped or parked by the spend gate. No network traffic happened.
    Block(SpendBlock),
    /// The call was made (or failed before sending) and the provider layer
    /// reported an error.
    Provider(ProviderError),
}

#[derive(Debug)]
pub struct MeteredOutcome {
    pub outcome: HostedOutcome,
    pub notices: Vec<LimitNotice>,
}

/// Pre-flight, call, settle. The ONLY way the executor reaches a hosted
/// provider.
pub async fn metered_generate<Tz: TimeZone>(
    client: &HostedClient,
    call_ctx: &CallContext,
    req: &HostedRequest,
    spend: &SpendCall<'_>,
    tz: &Tz,
) -> Result<MeteredOutcome, SpendCallError> {
    // The cap is computed from exactly the limit the request will carry.
    let output_limit = effective_output_limit(call_ctx, req)
        .ok_or_else(|| SpendCallError::Provider(missing_limit_error(call_ctx)))?;

    let mut conn = match store::open(spend.user_id, spend.key_hex).await {
        Ok(c) => c,
        Err(e) => {
            log::warn!("spend gate: could not open the spend store: {e}");
            return Err(SpendCallError::Block(SpendBlock::new(
                Code::BlockLedgerUnavailable,
            )));
        }
    };

    let prompt_bytes = (req.prompt.len() + req.system.as_ref().map_or(0, String::len)) as u64;
    let reserved = preflight(&mut conn, spend, prompt_bytes, output_limit, tz, Utc::now())
        .await
        .map_err(SpendCallError::Block)?;

    let attempt = client.generate_metered(call_ctx, req).await;
    finish(
        &mut conn,
        &reserved,
        spend.provider.id.as_str(),
        &attempt,
        Utc::now(),
    )
    .await;

    for n in &reserved.notices {
        log::info!(
            "spend: limit {} reached its warning threshold ({} {} {})",
            n.limit_id,
            n.metric.as_str(),
            n.period.as_str(),
            n.scope_kind.as_str()
        );
    }

    match attempt.result {
        Ok(outcome) => Ok(MeteredOutcome {
            outcome,
            notices: reserved.notices,
        }),
        Err(e) => Err(SpendCallError::Provider(e)),
    }
}

/// Failure metadata for a block: the code and its parameters, for
/// `FailureResult.metadata` (key `code`, plus every parameter).
pub fn block_metadata(block: &SpendBlock) -> Value {
    let mut m = block.params.clone();
    m.insert("code".to_owned(), json!(block.code_str()));
    Value::Object(m)
}

#[cfg(test)]
#[path = "gate_tests.rs"]
mod tests;
