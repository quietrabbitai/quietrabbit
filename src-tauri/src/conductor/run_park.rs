// src-tauri/src/conductor/run_park.rs
//
// items.id=901 stage 7: the PARK REASON of a run, built from the failure that parked it.
//
// A run that parks (F10 AwaitUser and the consent-style pauses) leaves its reason in
// FailureResult.metadata only, and no IPC path carries that to a screen. lifecycle writes the
// reason to run_parks (persistence::run_park_store) in the same function that writes the run's
// focus_runs row, and get_run_park (stage 8) reads it back. This module is the PURE part: which
// failures leave a park, what they carry, and how the parameters are fitted to the store's cap.
//
// WHICH FAILURES. Only a failure whose metadata `code` is a member of spend::codes::Code::ALL
// AND belongs to one of four families: the resolver (resolver.too_weak, resolver.no_candidate),
// the spend gate (spend_block.*), the plan (plan.exceeded) and the outbound step review
// (step_review.*). Any other code, a code this registry does not know, a failure with no code or
// no action id leaves no park. The match is over the Code enum, so a new family is a decision
// here, not an accident of a string prefix.
//
// WHAT IT CARRIES. The code, the step's action id and the failure's structured parameters,
// after a shape filter (sanitize_params): keys of lowercase letters, digits and underscores,
// values that are numbers, booleans, null, or short identifier-shaped words (letters, digits and
// . _ : / + -, no spaces), arrays and objects of those at most three deep. A value of any other
// shape (anything with a space or a newline, a long string) is DROPPED, so prose cannot ride in
// a park. This is a shape filter, not a semantic one: the guarantee that a parameter is an id
// rests on the producers (SpendBlock::with calls pass ids, counts and reason codes), and the
// filter is the second line.
//
// THE CAP AND THE TRIM (fit). run_park_store caps params_json at 4096 bytes. A resolver stop
// carries the options the resolver ranked (best first). When the JSON would not fit, whole
// options are removed from the END (the worst-ranked) until it does, `options_omitted` records
// how many of `options_total` are not carried, and the result is always complete, valid JSON.
// If removing every option is still not enough, only scalar parameters are kept, and if even
// those do not fit, `{}`. Partial JSON is never produced.
//
// LOGGING. Nothing here logs, and Debug on a ParkRecord shows the code only.

use std::collections::HashMap;
use std::fmt;

use serde_json::{Map, Value};

use crate::conductor::failure::FailureResult;
use crate::persistence::run_park_store::PARAMS_MAX_BYTES;
use crate::spend::codes::Code;

/// The parameter keys of a resolver stop that carry its options.
pub const OPTIONS_KEY: &str = "options";
pub const OPTIONS_TOTAL_KEY: &str = "options_total";
pub const OPTIONS_OMITTED_KEY: &str = "options_omitted";

const MAX_DEPTH: u8 = 3;
const MAX_ARRAY: usize = 32;
const MAX_KEY: usize = 40;
const MAX_WORD: usize = 128;

/// One park, ready for the store or the memory home.
#[derive(Clone, PartialEq, Eq)]
pub struct ParkRecord {
    pub code: Code,
    pub action_id: String,
    /// A complete JSON object of at most PARAMS_MAX_BYTES bytes.
    pub params_json: String,
}

impl fmt::Debug for ParkRecord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ParkRecord({})", self.code.as_str())
    }
}

/// The registered code a string names, if any.
pub fn registered_code(s: &str) -> Option<Code> {
    Code::ALL.iter().copied().find(|c| c.as_str() == s)
}

/// Whether a code is one a park may carry: the four families, by variant.
pub fn is_park_family(code: Code) -> bool {
    matches!(
        code,
        Code::ResolverTooWeak
            | Code::ResolverNoCandidate
            | Code::PlanExceeded
            | Code::StepReviewRequired
            | Code::StepReviewPrivacyFilterUnavailable
            | Code::StepReviewBlocked
            | Code::StepReviewRefused
            | Code::BlockConsentRequired
            | Code::BlockReconsentRequired
            | Code::BlockPriceUnknown
            | Code::BlockCapRequired
            | Code::BlockLimitReached
            | Code::BlockBudgetApprovalRequired
            | Code::BlockRateLimitedLongWait
            | Code::BlockLedgerUnavailable
            | Code::BlockModelRetired
            | Code::BlockModelUnavailable
            | Code::BlockModelOutputLimitRequired
    )
}

/// The park a failure leaves, or None when it leaves none.
pub fn from_failure(failure: &FailureResult) -> Option<ParkRecord> {
    let meta = failure.metadata.as_ref()?;
    let code = registered_code(meta.get("code")?.as_str()?)?;
    if !is_park_family(code) {
        return None;
    }
    let action_id = failure.action_id.as_deref().filter(|a| !a.is_empty())?;
    Some(ParkRecord {
        code,
        action_id: action_id.to_owned(),
        params_json: fit(sanitize_params(meta)),
    })
}

fn word_ok(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= MAX_WORD
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | ':' | '/' | '+' | '-'))
}

fn key_ok(k: &str) -> bool {
    !k.is_empty()
        && k.len() <= MAX_KEY
        && k.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

fn clean(v: &Value, depth: u8) -> Option<Value> {
    match v {
        Value::Null | Value::Bool(_) | Value::Number(_) => Some(v.clone()),
        Value::String(s) => word_ok(s).then(|| v.clone()),
        Value::Array(items) if depth < MAX_DEPTH => Some(Value::Array(
            items
                .iter()
                .take(MAX_ARRAY)
                .filter_map(|i| clean(i, depth + 1))
                .collect(),
        )),
        Value::Object(map) if depth < MAX_DEPTH => Some(Value::Object(clean_map(map, depth + 1))),
        _ => None,
    }
}

fn clean_map(map: &Map<String, Value>, depth: u8) -> Map<String, Value> {
    map.iter()
        .filter(|(k, _)| key_ok(k))
        .filter_map(|(k, v)| clean(v, depth).map(|c| (k.clone(), c)))
        .collect()
}

/// The failure's parameters that may be stored: the shape filter above, minus `code` (the
/// store has a column for it). Key order is not significant (a JSON object).
pub fn sanitize_params(meta: &HashMap<String, Value>) -> Map<String, Value> {
    let mut sorted: Vec<(&String, &Value)> = meta.iter().filter(|(k, _)| *k != "code").collect();
    // A fixed order, so the same failure always stores the same text.
    sorted.sort_by(|a, b| a.0.cmp(b.0));
    let mut out: Map<String, Value> = sorted
        .into_iter()
        .filter(|(k, _)| key_ok(k))
        .filter_map(|(k, v)| clean(v, 0).map(|c| (k.clone(), c)))
        .collect();
    // The array cap above must not hide how many options there were: the producer's own
    // total (or the length it handed over) is kept as the total.
    if let Some(Value::Array(given)) = meta.get(OPTIONS_KEY) {
        let told = meta
            .get(OPTIONS_TOTAL_KEY)
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let total = told.max(given.len() as u64);
        if out.contains_key(OPTIONS_KEY) {
            out.insert(OPTIONS_TOTAL_KEY.to_owned(), Value::from(total));
        }
    }
    out
}

fn text(m: &Map<String, Value>) -> String {
    serde_json::to_string(m).unwrap_or_else(|_| "{}".to_owned())
}

/// Fit the parameters to the store's cap. Always complete, valid JSON (see the module
/// comment): options are dropped from the end, then everything but scalars, then `{}`.
pub fn fit(mut params: Map<String, Value>) -> String {
    // Account for options the producer already left out, so a screen can say "and N more".
    if let Some(Value::Array(opts)) = params.get(OPTIONS_KEY) {
        let carried = opts.len() as u64;
        let total = params
            .get(OPTIONS_TOTAL_KEY)
            .and_then(Value::as_u64)
            .unwrap_or(carried)
            .max(carried);
        params.insert(OPTIONS_TOTAL_KEY.to_owned(), Value::from(total));
        if total > carried {
            params.insert(OPTIONS_OMITTED_KEY.to_owned(), Value::from(total - carried));
        }
    }
    let whole = text(&params);
    if whole.len() <= PARAMS_MAX_BYTES {
        return whole;
    }
    if let Some(Value::Array(opts)) = params.get(OPTIONS_KEY).cloned() {
        let total = params
            .get(OPTIONS_TOTAL_KEY)
            .and_then(Value::as_u64)
            .unwrap_or(opts.len() as u64);
        for keep in (0..opts.len()).rev() {
            params.insert(OPTIONS_KEY.to_owned(), Value::Array(opts[..keep].to_vec()));
            params.insert(
                OPTIONS_OMITTED_KEY.to_owned(),
                Value::from(total - keep as u64),
            );
            let t = text(&params);
            if t.len() <= PARAMS_MAX_BYTES {
                return t;
            }
        }
    }
    // Still too large: scalars only.
    params.retain(|_, v| !v.is_array() && !v.is_object());
    let t = text(&params);
    if t.len() <= PARAMS_MAX_BYTES {
        t
    } else {
        "{}".to_owned()
    }
}

#[cfg(test)]
#[path = "run_park_tests.rs"]
mod tests;
