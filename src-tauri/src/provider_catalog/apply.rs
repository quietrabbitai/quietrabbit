// src-tauri/src/provider_catalog/apply.rs
//
// Applying a catalog document to the shared.db rows, and undoing it
// (items.id=713; decisions.id=875 point 6).
//
// THE CALLER OWNS THE TRANSACTION. `apply_catalog` and `rollback` run on a
// connection that is already inside one. The preview command applies, reads
// the result and ROLLS THE TRANSACTION BACK, so the preview and the real apply
// are the same code and cannot disagree. The apply command commits.
//
// WHAT MAY CHANGE is only what the document format can express (format.rs) and
// only these columns, listed once below as PROVIDER_GOVERNED and
// MODEL_GOVERNED. EVERY OTHER COLUMN IS RELEASE-ONLY BY DEFAULT: after the
// apply, a fingerprint of every non-governed column of every row that existed
// before is compared with the one taken before, and any difference fails the
// whole apply. A column added by a later migration is therefore release-only
// until someone moves it into a governed list on purpose. Two columns are
// governed in part and are fingerprinted with the governed part removed:
// provider_models.request_options (the output-limit key) and
// providers.credential_acquisition (keys_page_url).
//
// THE TWO GUARANTEES the item names:
//   1. providers.required_request_options cannot be weakened. It is not in the
//      format, it is not governed, and it is asserted equal before and after
//      by name (a separate check from the fingerprint so the failure says what
//      it is).
//   2. A model's privacy protection cannot be removed. Non-limit
//      request_options and the three privacy overrides are not governed, so
//      the fingerprint covers them.
//
// CONSENT. The consent attributes hash (spend::disclosure::attributes_hash) is
// computed for every touched provider before and after. A changed hash means
// every user of that provider is asked to consent again; the outcome lists
// those providers with a reason. A hash that changed for a reason the apply
// cannot name (anything but a model added, a model leaving the live set, or a
// cost-class change) fails the apply: the catalog format must never change a
// disclosed privacy attribute. Retiring a model re-asks consent because the
// hash covers the non-retired set; that is how disclosure.rs is built
// (Chat-PM ruled to implement it as built) and the preview says so by code.
//
// NO PROVIDER OR MODEL IS NAMED HERE.

use std::collections::{BTreeMap, BTreeSet};

use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use sqlx::{Row, SqliteConnection};

use crate::persistence::provider_store::{
    self, ModelStatus, Provider, ProviderModel, ProviderStoreError,
};
use crate::provider_catalog::codes;
use crate::provider_catalog::format::{
    CatalogDoc, ModelUpdate, NewModel, PriceDoc, ProviderUpdate, StatusDoc,
};
use crate::spend::disclosure::{attributes_hash, cost_class_of, CostClass};
use crate::spend::money::parse_decimal_micros;
use crate::spend::price::parse_pricing;

/// Ceiling for one price, per million tokens, in micro-USD (1,000 USD): a guard
/// against unit mistakes, the same figure commands::provider_models uses.
pub const MAX_PRICE_MICROS_PER_MTOK: u64 = 1_000_000_000;
const MAX_TIERS: usize = 4;
const MAX_CONTEXT_TOKENS: u32 = 10_000_000;
const MAX_TEXT_CHARS: usize = 2000;
const MAX_FREE_TIER_BYTES: usize = 4096;
const MAX_PLAN_NAMES: usize = 20;
const MAX_PLAN_NAME_CHARS: usize = 64;
const MAX_MODEL_ID_LEN: usize = 200;
const LIMIT_KEYS: [&str; 2] = ["max_tokens", "max_completion_tokens"];
const MIN_TOKEN_FACTOR: f64 = 0.25;
const MAX_TOKEN_FACTOR: f64 = 4.0;
const MIN_OUTPUT_OVERHEAD: f64 = 1.0;
const MAX_OUTPUT_OVERHEAD: f64 = 8.0;
/// A date in a catalog may be at most this many days after today.
pub const MAX_FUTURE_DAYS: i64 = 2;

// ---------------------------------------------------------------------------
// Governed columns (the only ones a catalog may write)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Text,
    Int,
    Real,
}

const PROVIDER_GOVERNED: &[(&str, Kind)] = &[
    ("free_tier", Kind::Text),
    ("plan_inclusion", Kind::Text),
    ("last_reviewed_at", Kind::Text),
    ("review_trigger_note", Kind::Text),
    ("qr_recommended", Kind::Int),
    ("qr_recommendation_rank", Kind::Int),
    ("spend_limit_url", Kind::Text),
    ("credential_acquisition", Kind::Text),
];

const MODEL_GOVERNED: &[(&str, Kind)] = &[
    ("pricing", Kind::Text),
    ("context_window_tokens", Kind::Int),
    ("context_window_source", Kind::Text),
    ("context_window_as_of", Kind::Text),
    ("token_factor", Kind::Real),
    ("output_overhead", Kind::Real),
    ("request_options", Kind::Text),
    ("qr_recommended", Kind::Int),
    ("qr_recommendation_rank", Kind::Int),
    ("requires_paid", Kind::Int),
    ("is_default", Kind::Int),
    ("status", Kind::Text),
    ("retires_at", Kind::Text),
];

/// A reason the document cannot be applied. `code` is a stable string from
/// `codes`; `detail` carries ids and field names only, never prose.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reject {
    pub code: &'static str,
    pub detail: String,
}

fn reject<T>(code: &'static str, detail: impl Into<String>) -> Result<T, Reject> {
    Err(Reject {
        code,
        detail: detail.into(),
    })
}

impl From<sqlx::Error> for Reject {
    fn from(e: sqlx::Error) -> Self {
        log::warn!("provider_catalog: database error: {e}");
        Reject {
            code: codes::REJECT_DATABASE,
            detail: String::new(),
        }
    }
}

impl From<ProviderStoreError> for Reject {
    fn from(e: ProviderStoreError) -> Self {
        log::warn!("provider_catalog: provider store error: {e}");
        Reject {
            code: codes::REJECT_DATABASE,
            detail: String::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// Outcome
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub provider_id: String,
    pub model_id: Option<String>,
    pub field: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skip {
    pub code: &'static str,
    pub provider_id: String,
    pub model_id: Option<String>,
    pub field: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsentEffect {
    pub provider_id: String,
    /// `codes::CONSENT_*`, sorted, no duplicates.
    pub reasons: Vec<&'static str>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ModelImage {
    pub provider_id: String,
    pub columns: Map<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AddedModel {
    pub id: String,
    pub provider_id: String,
}

/// Before-images of what an apply touched. Rolling back restores them.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Undo {
    pub providers: BTreeMap<String, Map<String, Value>>,
    pub models: BTreeMap<String, ModelImage>,
    pub added_models: Vec<AddedModel>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Outcome {
    pub changes: Vec<Change>,
    pub skipped: Vec<Skip>,
    pub consent_effects: Vec<ConsentEffect>,
    pub undo: Undo,
}

impl Outcome {
    /// Nothing changed and nothing needs undoing.
    pub fn is_noop(&self) -> bool {
        self.changes.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Small validators
// ---------------------------------------------------------------------------

fn parse_date(s: &str) -> Option<NaiveDate> {
    if s.len() != 10 {
        return None;
    }
    NaiveDate::parse_from_str(s, "%Y-%m-%d").ok()
}

fn date_ok(s: &str, today: NaiveDate) -> bool {
    parse_date(s).is_some_and(|d| (d - today).num_days() <= MAX_FUTURE_DAYS)
}

fn text_ok(s: &str, max_chars: usize) -> bool {
    s.chars().count() <= max_chars && !s.chars().any(|c| c.is_control() && c != '\n')
}

/// `https://host/...` with a host, no credentials in the URL.
fn https_host(url: &str) -> Option<String> {
    if url.len() > 512 {
        return None;
    }
    let u = reqwest::Url::parse(url).ok()?;
    if u.scheme() != "https" || !u.username().is_empty() || u.password().is_some() {
        return None;
    }
    u.host_str().map(str::to_owned)
}

fn model_id_ok(id: &str) -> bool {
    !id.is_empty() && id.len() <= MAX_MODEL_ID_LEN && id.bytes().all(|b| (0x21..=0x7e).contains(&b))
}

fn valid_decimal(s: &str) -> Option<u64> {
    let micros = parse_decimal_micros(s)?;
    (micros <= MAX_PRICE_MICROS_PER_MTOK).then_some(micros)
}

fn json_text(v: &Value) -> String {
    serde_json::to_string(v).expect("a serde_json::Value always serializes")
}

fn same_json(stored: Option<&str>, new: &Value) -> bool {
    stored
        .and_then(|t| serde_json::from_str::<Value>(t).ok())
        .is_some_and(|v| &v == new)
}

// ---------------------------------------------------------------------------
// Pricing
// ---------------------------------------------------------------------------

/// The stored `pricing` text for a catalog price, in the shape shared_028.sql
/// seeds. Decimal strings become number lexemes verbatim.
fn render_pricing(p: &PriceDoc, today: NaiveDate) -> Result<String, Reject> {
    let bad = |d: &str| reject(codes::REJECT_BAD_PRICE, d.to_owned());
    if valid_decimal(&p.input_per_mtok).is_none() || valid_decimal(&p.output_per_mtok).is_none() {
        return bad("rate");
    }
    if !date_ok(&p.as_of, today) {
        return bad("as_of");
    }
    if let Some(u) = &p.source_url {
        if https_host(u).is_none() {
            return bad("source_url");
        }
    }
    if let Some(n) = &p.note {
        if !text_ok(n, MAX_TEXT_CHARS) {
            return bad("note");
        }
    }
    if p.tiers.len() > MAX_TIERS {
        return bad("tiers");
    }
    let mut tiers = Vec::new();
    for t in &p.tiers {
        if t.prompt_tokens_up_to.is_some() == t.prompt_tokens_over.is_some() {
            return bad("tier_bound");
        }
        if valid_decimal(&t.input_per_mtok).is_none() || valid_decimal(&t.output_per_mtok).is_none()
        {
            return bad("tier_rate");
        }
        let bound = match (t.prompt_tokens_up_to, t.prompt_tokens_over) {
            (Some(n), None) => format!("\"prompt_tokens_up_to\":{n}"),
            (None, Some(n)) => format!("\"prompt_tokens_over\":{n}"),
            _ => return bad("tier_bound"),
        };
        tiers.push(format!(
            "{{{bound},\"input_per_mtok\":{},\"output_per_mtok\":{}}}",
            t.input_per_mtok, t.output_per_mtok
        ));
    }
    let tiers_text = if tiers.is_empty() {
        "null".to_owned()
    } else {
        format!("[{}]", tiers.join(","))
    };
    let source_url = match &p.source_url {
        Some(u) => json_text(&json!(u)),
        None => "null".to_owned(),
    };
    let mut out = format!(
        "{{\"input_per_mtok\":{},\"output_per_mtok\":{},\"currency\":\"USD\",\"cache\":null,\
         \"tiers\":{tiers_text},\"as_of\":{},\"source_url\":{source_url},\"source\":\"curated\"",
        p.input_per_mtok,
        p.output_per_mtok,
        json_text(&json!(p.as_of)),
    );
    if let Some(n) = &p.note {
        out.push_str(&format!(",\"note\":{}", json_text(&json!(n))));
    }
    out.push('}');
    // The spend code must be able to read exactly what is about to be stored.
    if parse_pricing(&out, None).is_none() {
        return bad("unreadable");
    }
    Ok(out)
}

fn stored_as_of(pricing_text: Option<&str>) -> Option<NaiveDate> {
    let v: Value = serde_json::from_str(pricing_text?).ok()?;
    parse_date(v.get("as_of")?.as_str()?)
}

// ---------------------------------------------------------------------------
// Row access
// ---------------------------------------------------------------------------

enum Val {
    Text(Option<String>),
    Int(Option<i64>),
    Real(Option<f64>),
}

async fn update_row(
    conn: &mut SqliteConnection,
    table: &'static str,
    id: &str,
    sets: &[(&'static str, Val)],
) -> Result<(), sqlx::Error> {
    if sets.is_empty() {
        return Ok(());
    }
    let assignments: Vec<String> = sets.iter().map(|(c, _)| format!("{c} = ?")).collect();
    let sql = format!("UPDATE {table} SET {} WHERE id = ?", assignments.join(", "));
    let mut q = sqlx::query(&sql);
    for (_, v) in sets {
        q = match v {
            Val::Text(t) => q.bind(t.clone()),
            Val::Int(i) => q.bind(*i),
            Val::Real(r) => q.bind(*r),
        };
    }
    q.bind(id).execute(&mut *conn).await?;
    Ok(())
}

async fn read_image(
    conn: &mut SqliteConnection,
    table: &'static str,
    id: &str,
    cols: &[(&'static str, Kind)],
) -> Result<Option<Map<String, Value>>, sqlx::Error> {
    let names: Vec<&str> = cols.iter().map(|(c, _)| *c).collect();
    let sql = format!("SELECT {} FROM {table} WHERE id = ?", names.join(", "));
    let Some(row) = sqlx::query(&sql)
        .bind(id)
        .fetch_optional(&mut *conn)
        .await?
    else {
        return Ok(None);
    };
    let mut out = Map::new();
    for (name, kind) in cols {
        let v = match kind {
            Kind::Text => row
                .try_get::<Option<String>, _>(*name)?
                .map_or(Value::Null, Value::String),
            Kind::Int => row
                .try_get::<Option<i64>, _>(*name)?
                .map_or(Value::Null, |i| json!(i)),
            Kind::Real => row
                .try_get::<Option<f64>, _>(*name)?
                .map_or(Value::Null, |f| json!(f)),
        };
        out.insert((*name).to_owned(), v);
    }
    Ok(Some(out))
}

fn image_to_sets(
    image: &Map<String, Value>,
    cols: &[(&'static str, Kind)],
    skip_default: bool,
) -> Vec<(&'static str, Val)> {
    let mut sets = Vec::new();
    for (name, kind) in cols {
        if skip_default && *name == "is_default" {
            continue;
        }
        let v = image.get(*name).unwrap_or(&Value::Null);
        sets.push((
            *name,
            match kind {
                Kind::Text => Val::Text(v.as_str().map(str::to_owned)),
                Kind::Int => Val::Int(v.as_i64()),
                Kind::Real => Val::Real(v.as_f64()),
            },
        ));
    }
    sets
}

// ---------------------------------------------------------------------------
// Fingerprint of everything a catalog may not change
// ---------------------------------------------------------------------------

async fn table_columns(
    conn: &mut SqliteConnection,
    table: &'static str,
) -> Result<Vec<String>, sqlx::Error> {
    let rows = sqlx::query(&format!("PRAGMA table_info({table})"))
        .fetch_all(&mut *conn)
        .await?;
    rows.iter()
        .map(|r| r.try_get::<String, _>("name"))
        .collect()
}

fn without_limit_keys(text: Option<String>) -> String {
    match text.and_then(|t| serde_json::from_str::<Value>(&t).ok()) {
        Some(Value::Object(mut m)) => {
            for k in LIMIT_KEYS {
                m.remove(k);
            }
            json_text(&Value::Object(m))
        }
        Some(other) => json_text(&other),
        None => String::new(),
    }
}

fn without_keys_page_url(text: Option<String>) -> String {
    match text.and_then(|t| serde_json::from_str::<Value>(&t).ok()) {
        Some(Value::Object(mut m)) => {
            m.remove("keys_page_url");
            json_text(&Value::Object(m))
        }
        Some(other) => json_text(&other),
        None => String::new(),
    }
}

/// `table:id` -> a digest-ready string over every non-governed column.
pub(crate) async fn release_fingerprint(
    conn: &mut SqliteConnection,
) -> Result<BTreeMap<String, String>, sqlx::Error> {
    let mut out = BTreeMap::new();
    for (table, governed, special) in [
        ("providers", PROVIDER_GOVERNED, "credential_acquisition"),
        ("provider_models", MODEL_GOVERNED, "request_options"),
    ] {
        let governed_names: BTreeSet<&str> = governed.iter().map(|(c, _)| *c).collect();
        let plain: Vec<String> = table_columns(conn, table)
            .await?
            .into_iter()
            .filter(|c| !governed_names.contains(c.as_str()) && c != "id")
            .collect();
        let quoted: Vec<String> = plain.iter().map(|c| format!("quote({c})")).collect();
        let select_plain = if quoted.is_empty() {
            "''".to_owned()
        } else {
            quoted.join(" || char(31) || ")
        };
        let sql = format!("SELECT id, {select_plain} AS plain, {special} AS special FROM {table}");
        for row in sqlx::query(&sql).fetch_all(&mut *conn).await? {
            let id: String = row.try_get("id")?;
            let plain: String = row.try_get("plain")?;
            let special_text: Option<String> = row.try_get("special")?;
            let special_norm = if table == "providers" {
                without_keys_page_url(special_text)
            } else {
                without_limit_keys(special_text)
            };
            out.insert(
                format!("{table}:{id}"),
                format!("{plain}\u{1f}{special_norm}"),
            );
        }
    }
    Ok(out)
}

pub(crate) fn fingerprint_drift(
    before: &BTreeMap<String, String>,
    after: &BTreeMap<String, String>,
) -> Option<String> {
    for (k, v) in before {
        match after.get(k) {
            Some(a) if a == v => {}
            Some(_) => return Some(k.clone()),
            None => {
                // A row that existed before is gone. Only a rollback deletes
                // rows, and only rows the apply added, which are not in `before`.
                return Some(k.clone());
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Consent snapshots
// ---------------------------------------------------------------------------

pub(crate) struct ConsentSnapshot {
    pub(crate) hash: String,
    pub(crate) live: BTreeMap<String, CostClass>,
}

pub(crate) async fn consent_snapshot(
    conn: &mut SqliteConnection,
    provider_id: &str,
) -> Result<ConsentSnapshot, Reject> {
    let provider: Provider = provider_store::get_provider_conn(conn, provider_id)
        .await?
        .ok_or_else(|| Reject {
            code: codes::REJECT_UNKNOWN_PROVIDER,
            detail: provider_id.to_owned(),
        })?;
    let models: Vec<ProviderModel> =
        provider_store::list_models_for_provider_conn(conn, provider_id).await?;
    let texts = provider_store::list_model_pricing_text_conn(conn, provider_id).await?;
    let hash = attributes_hash(&provider, &models, &texts);
    let mut live = BTreeMap::new();
    for m in models.iter().filter(|m| m.status != ModelStatus::Retired) {
        let text = texts
            .iter()
            .find(|(id, _)| id == &m.model_id)
            .and_then(|(_, t)| t.as_deref());
        live.insert(m.model_id.clone(), cost_class_of(text));
    }
    Ok(ConsentSnapshot { hash, live })
}

pub(crate) fn consent_effect(
    provider_id: &str,
    before: &ConsentSnapshot,
    after: &ConsentSnapshot,
) -> Result<Option<ConsentEffect>, Reject> {
    if before.hash == after.hash {
        return Ok(None);
    }
    let mut reasons: BTreeSet<&'static str> = BTreeSet::new();
    for (id, class) in &after.live {
        match before.live.get(id) {
            None => {
                reasons.insert(codes::CONSENT_MODEL_ADDED);
            }
            Some(b) if b != class => {
                reasons.insert(codes::CONSENT_COST_CLASS_CHANGED);
            }
            Some(_) => {}
        }
    }
    for id in before.live.keys() {
        if !after.live.contains_key(id) {
            reasons.insert(codes::CONSENT_MODEL_RETIRED);
        }
    }
    if reasons.is_empty() {
        // The hash moved and nothing above explains it: a disclosed attribute
        // changed. The format cannot express that, so refuse the apply.
        return reject(codes::REJECT_UNEXPLAINED_CONSENT_CHANGE, provider_id);
    }
    Ok(Some(ConsentEffect {
        provider_id: provider_id.to_owned(),
        reasons: reasons.into_iter().collect(),
    }))
}

// ---------------------------------------------------------------------------
// Apply
// ---------------------------------------------------------------------------

struct Ctx {
    today: NaiveDate,
    out: Outcome,
    touched_providers: BTreeSet<String>,
    /// model row ids whose output limit must be re-checked at the end.
    touched_models: BTreeSet<String>,
}

impl Ctx {
    fn change(&mut self, provider_id: &str, model_id: Option<&str>, field: &'static str) {
        self.out.changes.push(Change {
            provider_id: provider_id.to_owned(),
            model_id: model_id.map(str::to_owned),
            field,
        });
    }

    fn skip(
        &mut self,
        code: &'static str,
        provider_id: &str,
        model_id: Option<&str>,
        field: &'static str,
    ) {
        self.out.skipped.push(Skip {
            code,
            provider_id: provider_id.to_owned(),
            model_id: model_id.map(str::to_owned),
            field,
        });
    }
}

async fn require_hosted(conn: &mut SqliteConnection, provider_id: &str) -> Result<(), Reject> {
    let row = sqlx::query("SELECT is_local, api_dialect FROM providers WHERE id = ?")
        .bind(provider_id)
        .fetch_optional(&mut *conn)
        .await?;
    let Some(row) = row else {
        return reject(codes::REJECT_UNKNOWN_PROVIDER, provider_id);
    };
    let is_local: i64 = row.try_get("is_local")?;
    let dialect: Option<String> = row.try_get("api_dialect")?;
    if is_local != 0 || dialect.is_none() {
        return reject(codes::REJECT_NOT_HOSTED_PROVIDER, provider_id);
    }
    Ok(())
}

fn model_row_id(provider_id: &str, model_id: &str) -> String {
    format!("{provider_id}:{model_id}")
}

/// Apply `doc` to the rows on `conn`, which must already be inside a
/// transaction. On `Err` the caller rolls the transaction back.
pub async fn apply_catalog(
    conn: &mut SqliteConnection,
    doc: &CatalogDoc,
    today: NaiveDate,
) -> Result<Outcome, Reject> {
    // Duplicate entries would make the order of application matter.
    let mut seen_p = BTreeSet::new();
    for p in &doc.providers {
        if !seen_p.insert(p.id.as_str()) {
            return reject(codes::REJECT_DUPLICATE_ENTRY, p.id.clone());
        }
    }
    let mut seen_m = BTreeSet::new();
    for m in &doc.models {
        if !seen_m.insert((m.provider_id.as_str(), m.model_id.as_str())) {
            return reject(
                codes::REJECT_DUPLICATE_ENTRY,
                model_row_id(&m.provider_id, &m.model_id),
            );
        }
    }
    for n in &doc.new_models {
        if !seen_m.insert((n.provider_id.as_str(), n.model_id.as_str())) {
            return reject(
                codes::REJECT_DUPLICATE_ENTRY,
                model_row_id(&n.provider_id, &n.model_id),
            );
        }
    }

    let mut ctx = Ctx {
        today,
        out: Outcome::default(),
        touched_providers: BTreeSet::new(),
        touched_models: BTreeSet::new(),
    };
    for p in &doc.providers {
        ctx.touched_providers.insert(p.id.clone());
    }
    for m in &doc.models {
        ctx.touched_providers.insert(m.provider_id.clone());
    }
    for n in &doc.new_models {
        ctx.touched_providers.insert(n.provider_id.clone());
    }
    let touched: Vec<String> = ctx.touched_providers.iter().cloned().collect();
    for id in &touched {
        require_hosted(conn, id).await?;
    }

    let fp_before = release_fingerprint(conn).await?;
    let required_before = required_options_all(conn).await?;
    let default_before = defaults_of(conn, &touched).await?;
    let mut consent_before = BTreeMap::new();
    for id in &touched {
        consent_before.insert(id.clone(), consent_snapshot(conn, id).await?);
    }

    for u in &doc.providers {
        apply_provider(conn, &mut ctx, u).await?;
    }
    for u in &doc.models {
        apply_model(conn, &mut ctx, u).await?;
    }
    for n in &doc.new_models {
        add_model(conn, &mut ctx, n).await?;
    }

    // -- invariants ---------------------------------------------------------
    // Named first so the failure says what it is, then the general fingerprint.
    if required_options_all(conn).await? != required_before {
        return reject(codes::REJECT_REQUIRED_OPTIONS_CHANGED, "");
    }
    let fp_after = release_fingerprint(conn).await?;
    if let Some(key) = fingerprint_drift(&fp_before, &fp_after) {
        return reject(codes::REJECT_RELEASE_ONLY_CHANGED, key);
    }
    check_limits(conn, &ctx.touched_models).await?;
    check_defaults(conn, &default_before).await?;

    let mut effects = Vec::new();
    for id in &touched {
        let after = consent_snapshot(conn, id).await?;
        if let Some(e) = consent_effect(id, &consent_before[id], &after)? {
            effects.push(e);
        }
    }
    ctx.out.consent_effects = effects;
    Ok(ctx.out)
}

async fn required_options_all(
    conn: &mut SqliteConnection,
) -> Result<BTreeMap<String, Option<String>>, sqlx::Error> {
    let rows = sqlx::query("SELECT id, required_request_options FROM providers")
        .fetch_all(&mut *conn)
        .await?;
    let mut out = BTreeMap::new();
    for r in rows {
        out.insert(r.try_get("id")?, r.try_get("required_request_options")?);
    }
    Ok(out)
}

/// provider id -> the model row id that was default (None = no default).
async fn defaults_of(
    conn: &mut SqliteConnection,
    providers: &[String],
) -> Result<BTreeMap<String, Option<String>>, sqlx::Error> {
    let mut out = BTreeMap::new();
    for p in providers {
        let id: Option<String> = sqlx::query_scalar(
            "SELECT id FROM provider_models WHERE provider_id = ? AND is_default = 1",
        )
        .bind(p)
        .fetch_optional(&mut *conn)
        .await?;
        out.insert(p.clone(), id);
    }
    Ok(out)
}

async fn check_defaults(
    conn: &mut SqliteConnection,
    before: &BTreeMap<String, Option<String>>,
) -> Result<(), Reject> {
    for (provider, had) in before {
        if had.is_none() {
            continue;
        }
        let rows = sqlx::query(
            "SELECT status FROM provider_models WHERE provider_id = ? AND is_default = 1",
        )
        .bind(provider)
        .fetch_all(&mut *conn)
        .await?;
        if rows.len() != 1 {
            return reject(codes::REJECT_DEFAULT_MODEL, provider.clone());
        }
        let status: String = rows[0].try_get("status")?;
        if status == "retired" {
            return reject(codes::REJECT_DEFAULT_MODEL, provider.clone());
        }
    }
    Ok(())
}

async fn check_limits(
    conn: &mut SqliteConnection,
    model_ids: &BTreeSet<String>,
) -> Result<(), Reject> {
    for id in model_ids {
        let row = sqlx::query(
            "SELECT request_options, context_window_tokens, pricing
               FROM provider_models WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(&mut *conn)
        .await?;
        let Some(row) = row else { continue };
        let options: Option<String> = row.try_get("request_options")?;
        let context: i64 = row.try_get("context_window_tokens")?;
        let pricing: Option<String> = row.try_get("pricing")?;
        let opts: Value = options
            .as_deref()
            .and_then(|t| serde_json::from_str(t).ok())
            .unwrap_or(Value::Null);
        let limits: Vec<i64> = LIMIT_KEYS
            .iter()
            .filter_map(|k| opts.get(*k).and_then(Value::as_i64))
            .collect();
        if limits.len() != 1 || limits[0] < 1 || (context > 0 && limits[0] > context) {
            return reject(codes::REJECT_OUTPUT_LIMIT, id.clone());
        }
        if pricing
            .as_deref()
            .and_then(|t| parse_pricing(t, None))
            .is_none()
        {
            return reject(codes::REJECT_BAD_PRICE, id.clone());
        }
    }
    Ok(())
}

// -- providers ---------------------------------------------------------------

fn validate_free_tier(m: &Map<String, Value>, today: NaiveDate) -> Result<(), Reject> {
    let bad = |d: &str| reject(codes::REJECT_BAD_VALUE, format!("free_tier.{d}"));
    if !matches!(m.get("available"), Some(Value::Bool(_))) {
        return bad("available");
    }
    match m.get("as_of").and_then(Value::as_str) {
        Some(d) if date_ok(d, today) => {}
        _ => return bad("as_of"),
    }
    if let Some(v) = m.get("source_url") {
        match v {
            Value::Null => {}
            Value::String(u) if https_host(u).is_some() => {}
            _ => return bad("source_url"),
        }
    }
    fn shallow(v: &Value, depth: usize) -> bool {
        match v {
            Value::Null | Value::Bool(_) | Value::Number(_) => true,
            Value::String(s) => text_ok(s, MAX_TEXT_CHARS),
            Value::Object(o) if depth < 2 => o.values().all(|x| shallow(x, depth + 1)),
            _ => false,
        }
    }
    if !m.values().all(|v| shallow(v, 0)) {
        return bad("shape");
    }
    if json_text(&Value::Object(m.clone())).len() > MAX_FREE_TIER_BYTES {
        return bad("size");
    }
    Ok(())
}

async fn apply_provider(
    conn: &mut SqliteConnection,
    ctx: &mut Ctx,
    u: &ProviderUpdate,
) -> Result<(), Reject> {
    let id = u.id.as_str();
    let row = sqlx::query(
        "SELECT free_tier, plan_inclusion, last_reviewed_at, review_trigger_note,
                qr_recommended, qr_recommendation_rank, spend_limit_url,
                credential_acquisition
           FROM providers WHERE id = ?",
    )
    .bind(id)
    .fetch_one(&mut *conn)
    .await?;
    let cur_free: Option<String> = row.try_get("free_tier")?;
    let cur_plan: Option<String> = row.try_get("plan_inclusion")?;
    let cur_reviewed: Option<String> = row.try_get("last_reviewed_at")?;
    let cur_note: Option<String> = row.try_get("review_trigger_note")?;
    let cur_rec: i64 = row.try_get("qr_recommended")?;
    let cur_rank: Option<i64> = row.try_get("qr_recommendation_rank")?;
    let cur_spend: Option<String> = row.try_get("spend_limit_url")?;
    let cur_cred: Option<String> = row.try_get("credential_acquisition")?;

    let mut sets: Vec<(&'static str, Val)> = Vec::new();
    let mut fields: Vec<&'static str> = Vec::new();

    if let Some(m) = &u.free_tier {
        validate_free_tier(m, ctx.today)?;
        let v = Value::Object(m.clone());
        if !same_json(cur_free.as_deref(), &v) {
            sets.push(("free_tier", Val::Text(Some(json_text(&v)))));
            fields.push("free_tier");
        }
    }
    if let Some(plans) = &u.plan_inclusion {
        let ok = plans.len() <= MAX_PLAN_NAMES
            && plans
                .iter()
                .all(|p| !p.is_empty() && text_ok(p, MAX_PLAN_NAME_CHARS) && !p.contains('\n'));
        if !ok {
            return reject(codes::REJECT_BAD_VALUE, "plan_inclusion");
        }
        let v = json!(plans);
        if !same_json(cur_plan.as_deref(), &v) {
            sets.push(("plan_inclusion", Val::Text(Some(json_text(&v)))));
            fields.push("plan_inclusion");
        }
    }
    if let Some(d) = &u.last_reviewed_at {
        if !date_ok(d, ctx.today) {
            return reject(codes::REJECT_BAD_VALUE, "last_reviewed_at");
        }
        if cur_reviewed.as_deref() != Some(d.as_str()) {
            sets.push(("last_reviewed_at", Val::Text(Some(d.clone()))));
            fields.push("last_reviewed_at");
        }
    }
    if let Some(n) = &u.review_trigger_note {
        if !text_ok(n, MAX_TEXT_CHARS) {
            return reject(codes::REJECT_BAD_VALUE, "review_trigger_note");
        }
        if cur_note.as_deref() != Some(n.as_str()) {
            sets.push(("review_trigger_note", Val::Text(Some(n.clone()))));
            fields.push("review_trigger_note");
        }
    }
    if let Some(b) = u.qr_recommended {
        if (cur_rec != 0) != b {
            sets.push(("qr_recommended", Val::Int(Some(i64::from(b)))));
            fields.push("qr_recommended");
        }
    }
    if let Some(r) = u.qr_recommendation_rank {
        if cur_rank != Some(i64::from(r)) {
            sets.push(("qr_recommendation_rank", Val::Int(Some(i64::from(r)))));
            fields.push("qr_recommendation_rank");
        }
    }
    if let Some(url) = &u.spend_limit_url {
        let Some(new_host) = https_host(url) else {
            return reject(codes::REJECT_BAD_URL, format!("{id}.spend_limit_url"));
        };
        let cur_host = cur_spend.as_deref().and_then(https_host);
        if cur_host.as_deref() != Some(new_host.as_str()) {
            return reject(
                codes::REJECT_URL_HOST_CHANGED,
                format!("{id}.spend_limit_url"),
            );
        }
        if cur_spend.as_deref() != Some(url.as_str()) {
            sets.push(("spend_limit_url", Val::Text(Some(url.clone()))));
            fields.push("spend_limit_url");
        }
    }
    if let Some(url) = &u.keys_page_url {
        let Some(new_host) = https_host(url) else {
            return reject(codes::REJECT_BAD_URL, format!("{id}.keys_page_url"));
        };
        let mut cred: Map<String, Value> = match cur_cred
            .as_deref()
            .and_then(|t| serde_json::from_str::<Value>(t).ok())
        {
            Some(Value::Object(m)) => m,
            _ => {
                return reject(
                    codes::REJECT_URL_HOST_CHANGED,
                    format!("{id}.keys_page_url"),
                )
            }
        };
        let cur_host = cred
            .get("keys_page_url")
            .and_then(Value::as_str)
            .and_then(https_host);
        if cur_host.as_deref() != Some(new_host.as_str()) {
            return reject(
                codes::REJECT_URL_HOST_CHANGED,
                format!("{id}.keys_page_url"),
            );
        }
        if cred.get("keys_page_url").and_then(Value::as_str) != Some(url.as_str()) {
            cred.insert("keys_page_url".to_owned(), json!(url));
            sets.push((
                "credential_acquisition",
                Val::Text(Some(json_text(&Value::Object(cred)))),
            ));
            fields.push("keys_page_url");
        }
    }

    if sets.is_empty() {
        return Ok(());
    }
    if !ctx.out.undo.providers.contains_key(id) {
        let image = read_image(conn, "providers", id, PROVIDER_GOVERNED)
            .await?
            .unwrap_or_default();
        ctx.out.undo.providers.insert(id.to_owned(), image);
    }
    update_row(conn, "providers", id, &sets).await?;
    for f in fields {
        ctx.change(id, None, f);
    }
    Ok(())
}

// -- models ------------------------------------------------------------------

async fn capture_model_image(
    conn: &mut SqliteConnection,
    ctx: &mut Ctx,
    row_id: &str,
    provider_id: &str,
) -> Result<(), Reject> {
    if ctx.out.undo.models.contains_key(row_id) {
        return Ok(());
    }
    if let Some(columns) = read_image(conn, "provider_models", row_id, MODEL_GOVERNED).await? {
        ctx.out.undo.models.insert(
            row_id.to_owned(),
            ModelImage {
                provider_id: provider_id.to_owned(),
                columns,
            },
        );
    }
    Ok(())
}

/// Make `row_id` the provider's only default model.
async fn move_default(
    conn: &mut SqliteConnection,
    ctx: &mut Ctx,
    provider_id: &str,
    row_id: &str,
) -> Result<(), Reject> {
    let others: Vec<String> = sqlx::query_scalar(
        "SELECT id FROM provider_models WHERE provider_id = ? AND is_default = 1 AND id <> ?",
    )
    .bind(provider_id)
    .bind(row_id)
    .fetch_all(&mut *conn)
    .await?;
    for other in others {
        capture_model_image(conn, ctx, &other, provider_id).await?;
        update_row(
            conn,
            "provider_models",
            &other,
            &[("is_default", Val::Int(Some(0)))],
        )
        .await?;
    }
    Ok(())
}

fn factor_ok(v: f64, lo: f64, hi: f64) -> bool {
    v.is_finite() && v >= lo && v <= hi
}

async fn apply_model(
    conn: &mut SqliteConnection,
    ctx: &mut Ctx,
    u: &ModelUpdate,
) -> Result<(), Reject> {
    let pid = u.provider_id.as_str();
    let mid = u.model_id.as_str();
    let row_id = model_row_id(pid, mid);
    let row = sqlx::query(
        "SELECT id, pricing, context_window_tokens, context_window_as_of, request_options,
                token_factor, output_overhead, qr_recommended, qr_recommendation_rank,
                requires_paid, is_default, status, retires_at
           FROM provider_models WHERE provider_id = ? AND model_id = ?",
    )
    .bind(pid)
    .bind(mid)
    .fetch_optional(&mut *conn)
    .await?;
    let Some(row) = row else {
        return reject(codes::REJECT_UNKNOWN_MODEL, row_id);
    };
    let db_id: String = row.try_get("id")?;
    let cur_pricing: Option<String> = row.try_get("pricing")?;
    let cur_context: i64 = row.try_get("context_window_tokens")?;
    let cur_context_as_of: Option<String> = row.try_get("context_window_as_of")?;
    let cur_options: Option<String> = row.try_get("request_options")?;
    let cur_factor: f64 = row.try_get("token_factor")?;
    let cur_overhead: f64 = row.try_get("output_overhead")?;
    let cur_rec: i64 = row.try_get("qr_recommended")?;
    let cur_rank: Option<i64> = row.try_get("qr_recommendation_rank")?;
    let cur_paid: i64 = row.try_get("requires_paid")?;
    let cur_default: i64 = row.try_get("is_default")?;
    let cur_status: String = row.try_get("status")?;
    let cur_retires: Option<String> = row.try_get("retires_at")?;

    let mut sets: Vec<(&'static str, Val)> = Vec::new();
    let mut fields: Vec<&'static str> = Vec::new();
    let mut default_to_set = false;

    if let Some(p) = &u.pricing {
        let text = render_pricing(p, ctx.today)?;
        let new_as_of = parse_date(&p.as_of);
        let older = matches!(
            (new_as_of, stored_as_of(cur_pricing.as_deref())),
            (Some(n), Some(c)) if n < c
        );
        if older {
            ctx.skip(codes::SKIP_PRICE_OLDER, pid, Some(mid), "pricing");
        } else {
            let new_v: Value = serde_json::from_str(&text).expect("rendered pricing is JSON");
            if !same_json(cur_pricing.as_deref(), &new_v) {
                sets.push(("pricing", Val::Text(Some(text))));
                fields.push("pricing");
            }
        }
    }
    if let Some(c) = &u.context_window {
        if c.tokens == 0 || c.tokens > MAX_CONTEXT_TOKENS || !date_ok(&c.as_of, ctx.today) {
            return reject(codes::REJECT_BAD_VALUE, format!("{row_id}.context_window"));
        }
        let older = matches!(
            (parse_date(&c.as_of), cur_context_as_of.as_deref().and_then(parse_date)),
            (Some(n), Some(cur)) if n < cur
        );
        if older {
            ctx.skip(codes::SKIP_CONTEXT_OLDER, pid, Some(mid), "context_window");
        } else if i64::from(c.tokens) != cur_context
            || cur_context_as_of.as_deref() != Some(c.as_of.as_str())
        {
            sets.push(("context_window_tokens", Val::Int(Some(i64::from(c.tokens)))));
            sets.push((
                "context_window_source",
                Val::Text(Some("curated".to_owned())),
            ));
            sets.push(("context_window_as_of", Val::Text(Some(c.as_of.clone()))));
            fields.push("context_window");
        }
    }
    if let Some(f) = u.token_factor {
        if !factor_ok(f, MIN_TOKEN_FACTOR, MAX_TOKEN_FACTOR) {
            return reject(codes::REJECT_BAD_VALUE, format!("{row_id}.token_factor"));
        }
        if f != cur_factor {
            sets.push(("token_factor", Val::Real(Some(f))));
            fields.push("token_factor");
        }
    }
    if let Some(o) = u.output_overhead {
        if !factor_ok(o, MIN_OUTPUT_OVERHEAD, MAX_OUTPUT_OVERHEAD) {
            return reject(codes::REJECT_BAD_VALUE, format!("{row_id}.output_overhead"));
        }
        if o != cur_overhead {
            sets.push(("output_overhead", Val::Real(Some(o))));
            fields.push("output_overhead");
        }
    }
    if let Some(tokens) = u.output_limit_tokens {
        let mut opts: Map<String, Value> = match cur_options
            .as_deref()
            .and_then(|t| serde_json::from_str::<Value>(t).ok())
        {
            Some(Value::Object(m)) => m,
            _ => return reject(codes::REJECT_OUTPUT_LIMIT, row_id),
        };
        let present: Vec<&str> = LIMIT_KEYS
            .iter()
            .copied()
            .filter(|k| opts.contains_key(*k))
            .collect();
        if present.len() != 1 || tokens == 0 {
            return reject(codes::REJECT_OUTPUT_LIMIT, row_id);
        }
        if opts.get(present[0]).and_then(Value::as_i64) != Some(i64::from(tokens)) {
            opts.insert(present[0].to_owned(), json!(tokens));
            sets.push((
                "request_options",
                Val::Text(Some(json_text(&Value::Object(opts)))),
            ));
            fields.push("output_limit_tokens");
        }
    }
    if let Some(b) = u.qr_recommended {
        if (cur_rec != 0) != b {
            sets.push(("qr_recommended", Val::Int(Some(i64::from(b)))));
            fields.push("qr_recommended");
        }
    }
    if let Some(r) = u.qr_recommendation_rank {
        if cur_rank != Some(i64::from(r)) {
            sets.push(("qr_recommendation_rank", Val::Int(Some(i64::from(r)))));
            fields.push("qr_recommendation_rank");
        }
    }
    if let Some(b) = u.requires_paid {
        if (cur_paid != 0) != b {
            sets.push(("requires_paid", Val::Int(Some(i64::from(b)))));
            fields.push("requires_paid");
        }
    }
    if let Some(d) = u.is_default {
        if (cur_default != 0) != d {
            if d {
                default_to_set = true;
            }
            sets.push(("is_default", Val::Int(Some(i64::from(d)))));
            fields.push("is_default");
        }
    }
    if let Some(s) = u.status {
        if s.as_str() != cur_status {
            sets.push(("status", Val::Text(Some(s.as_str().to_owned()))));
            fields.push("status");
        }
    }
    if let Some(r) = &u.retires_at {
        if let Some(d) = r {
            if parse_date(d).is_none() {
                return reject(codes::REJECT_BAD_VALUE, format!("{row_id}.retires_at"));
            }
        }
        if r.as_deref() != cur_retires.as_deref() {
            sets.push(("retires_at", Val::Text(r.clone())));
            fields.push("retires_at");
        }
    }

    ctx.touched_models.insert(db_id.clone());
    if sets.is_empty() {
        return Ok(());
    }
    capture_model_image(conn, ctx, &db_id, pid).await?;
    if default_to_set {
        move_default(conn, ctx, pid, &db_id).await?;
    }
    update_row(conn, "provider_models", &db_id, &sets).await?;
    for f in fields {
        ctx.change(pid, Some(mid), f);
    }
    Ok(())
}

async fn add_model(conn: &mut SqliteConnection, ctx: &mut Ctx, n: &NewModel) -> Result<(), Reject> {
    let pid = n.provider_id.as_str();
    let mid = n.model_id.as_str();
    let row_id = model_row_id(pid, mid);
    if !model_id_ok(mid) {
        return reject(codes::REJECT_BAD_VALUE, format!("{row_id}.model_id"));
    }
    let exists: Option<i64> = sqlx::query_scalar(
        "SELECT 1 FROM provider_models WHERE id = ? OR (provider_id = ? AND model_id = ?)",
    )
    .bind(&row_id)
    .bind(pid)
    .bind(mid)
    .fetch_optional(&mut *conn)
    .await?;
    if exists.is_some() {
        return reject(codes::REJECT_MODEL_EXISTS, row_id);
    }
    if !LIMIT_KEYS.contains(&n.output_limit.key.as_str()) || n.output_limit.tokens == 0 {
        return reject(codes::REJECT_OUTPUT_LIMIT, row_id);
    }
    let c = &n.context_window;
    if c.tokens == 0 || c.tokens > MAX_CONTEXT_TOKENS || !date_ok(&c.as_of, ctx.today) {
        return reject(codes::REJECT_BAD_VALUE, format!("{row_id}.context_window"));
    }
    let factor = n.token_factor.unwrap_or(1.0);
    let overhead = n.output_overhead.unwrap_or(1.0);
    if !factor_ok(factor, MIN_TOKEN_FACTOR, MAX_TOKEN_FACTOR)
        || !factor_ok(overhead, MIN_OUTPUT_OVERHEAD, MAX_OUTPUT_OVERHEAD)
    {
        return reject(codes::REJECT_BAD_VALUE, format!("{row_id}.factor"));
    }
    let status = match n.status {
        None | Some(StatusDoc::Active) => "active",
        Some(StatusDoc::Deprecated) => "deprecated",
        Some(StatusDoc::Retired) => {
            return reject(codes::REJECT_BAD_VALUE, format!("{row_id}.status"))
        }
    };
    if let Some(d) = &n.retires_at {
        if parse_date(d).is_none() {
            return reject(codes::REJECT_BAD_VALUE, format!("{row_id}.retires_at"));
        }
    }
    let pricing = render_pricing(&n.pricing, ctx.today)?;
    let options = json_text(&json!({ n.output_limit.key.as_str(): n.output_limit.tokens }));

    if n.is_default {
        move_default(conn, ctx, pid, &row_id).await?;
    }
    sqlx::query(
        "INSERT INTO provider_models
            (id, provider_id, model_id, context_window_tokens, is_default, created_at,
             pricing, request_options, token_factor, output_overhead, status, retires_at,
             requires_paid, qr_recommended, qr_recommendation_rank,
             context_window_source, context_window_as_of)
         VALUES (?, ?, ?, ?, ?, datetime('now'), ?, ?, ?, ?, ?, ?, ?, ?, ?, 'curated', ?)",
    )
    .bind(&row_id)
    .bind(pid)
    .bind(mid)
    .bind(i64::from(c.tokens))
    .bind(i64::from(n.is_default))
    .bind(&pricing)
    .bind(&options)
    .bind(factor)
    .bind(overhead)
    .bind(status)
    .bind(n.retires_at.clone())
    .bind(i64::from(n.requires_paid))
    .bind(i64::from(n.qr_recommended))
    .bind(n.qr_recommendation_rank.map(i64::from))
    .bind(&c.as_of)
    .execute(&mut *conn)
    .await?;
    ctx.out.undo.added_models.push(AddedModel {
        id: row_id.clone(),
        provider_id: pid.to_owned(),
    });
    ctx.touched_models.insert(row_id);
    ctx.change(pid, Some(mid), "new_model");
    Ok(())
}

// ---------------------------------------------------------------------------
// Rollback
// ---------------------------------------------------------------------------

/// Restore the before-images in `undo`. Inside the caller's transaction, like
/// `apply_catalog`. The same release-only fingerprint and consent comparison
/// run, so a rollback cannot change a non-governed column either.
pub async fn rollback(conn: &mut SqliteConnection, undo: &Undo) -> Result<Outcome, Reject> {
    let mut providers: BTreeSet<String> = BTreeSet::new();
    providers.extend(undo.providers.keys().cloned());
    providers.extend(undo.models.values().map(|m| m.provider_id.clone()));
    providers.extend(undo.added_models.iter().map(|a| a.provider_id.clone()));
    let touched: Vec<String> = providers.into_iter().collect();

    let fp_before = release_fingerprint(conn).await?;
    let required_before = required_options_all(conn).await?;
    let mut consent_before = BTreeMap::new();
    for id in &touched {
        consent_before.insert(id.clone(), consent_snapshot(conn, id).await?);
    }

    // Added models go first: it frees the one-default-per-provider slot.
    for a in &undo.added_models {
        sqlx::query("DELETE FROM provider_models WHERE id = ?")
            .bind(&a.id)
            .execute(&mut *conn)
            .await?;
    }
    for (id, image) in &undo.providers {
        update_row(
            conn,
            "providers",
            id,
            &image_to_sets(image, PROVIDER_GOVERNED, false),
        )
        .await?;
    }
    // Two passes for the default flag so the unique index is never violated.
    for (id, image) in &undo.models {
        update_row(
            conn,
            "provider_models",
            id,
            &image_to_sets(&image.columns, MODEL_GOVERNED, true),
        )
        .await?;
        update_row(
            conn,
            "provider_models",
            id,
            &[("is_default", Val::Int(Some(0)))],
        )
        .await?;
    }
    for (id, image) in &undo.models {
        if image.columns.get("is_default").and_then(Value::as_i64) == Some(1) {
            update_row(
                conn,
                "provider_models",
                id,
                &[("is_default", Val::Int(Some(1)))],
            )
            .await?;
        }
    }

    if required_options_all(conn).await? != required_before {
        return reject(codes::REJECT_REQUIRED_OPTIONS_CHANGED, "");
    }
    let fp_after = release_fingerprint(conn).await?;
    let mut fp_before_kept = fp_before.clone();
    for a in &undo.added_models {
        fp_before_kept.remove(&format!("provider_models:{}", a.id));
    }
    if let Some(key) = fingerprint_drift(&fp_before_kept, &fp_after) {
        return reject(codes::REJECT_RELEASE_ONLY_CHANGED, key);
    }
    let mut out = Outcome::default();
    for id in &touched {
        let after = consent_snapshot(conn, id).await?;
        if let Some(e) = consent_effect(id, &consent_before[id], &after)? {
            out.consent_effects.push(e);
        }
    }
    for id in undo.providers.keys() {
        out.changes.push(Change {
            provider_id: id.clone(),
            model_id: None,
            field: "restored",
        });
    }
    for (id, image) in &undo.models {
        out.changes.push(Change {
            provider_id: image.provider_id.clone(),
            model_id: Some(id.clone()),
            field: "restored",
        });
    }
    for a in &undo.added_models {
        out.changes.push(Change {
            provider_id: a.provider_id.clone(),
            model_id: Some(a.id.clone()),
            field: "removed",
        });
    }
    Ok(out)
}

#[cfg(test)]
pub(crate) fn governed_provider_columns() -> Vec<&'static str> {
    PROVIDER_GOVERNED.iter().map(|(c, _)| *c).collect()
}

#[cfg(test)]
pub(crate) fn governed_model_columns() -> Vec<&'static str> {
    MODEL_GOVERNED.iter().map(|(c, _)| *c).collect()
}
