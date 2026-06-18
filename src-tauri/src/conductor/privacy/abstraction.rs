// src-tauri/src/conductor/privacy/abstraction.rs
//
// Pure function -- no I/O, no async, no side effects.
// Mirrors Python's _apply_abstraction() in privacy.py exactly.
//
// Hotspot 1: range_only numeric banding.
//   Python uses int((numeric * 0.80) / 5000) * 5000 which truncates toward
//   zero (not floor). Rust i64 casting from f64 also truncates toward zero.
//   We replicate this exactly -- including the "0-0" result for negatives.
//   This is oracle-confirmed behaviour, not a bug to fix here.
//   Flagged to Chat-PM: negative financial values producing "0-0" is unspecified
//   behaviour in the architecture. Low practical risk (no UI allows negative
//   financial field entry). Address post-migration if needed.
//
// Hotspot 2: fail-safe defaults.
//   Unknown policy -> None (omit). Matches Python's final `return None`.
//   tier <= 1 -> return raw field_value regardless of policy.

use super::types::{AbstractionPolicy, PersonalField, Sensitivity};

/// Apply abstraction policy for the given tier.
/// Returns Some(abstracted_value) or None if the field should be omitted.
/// `tier` is abstraction_tier -- never execution_tier.
pub fn apply_abstraction(field: &PersonalField, tier: u8) -> Option<String> {
    // Tier 1: always return raw value regardless of policy.
    if tier <= 1 {
        return Some(field.field_value.clone());
    }

    let policy = if tier == 2 {
        &field.abstraction_tier2
    } else {
        &field.abstraction_tier3
    };

    match policy {
        AbstractionPolicy::Pass => Some(field.field_value.clone()),

        AbstractionPolicy::Omit => None,

        // not_permitted: non-blocking withhold at tier 2+ (D6-162, ADR-012 Amendment 1).
        // Returns None (omit) -- does NOT raise/panic. Gate1 adds field to withheld_fields.
        AbstractionPolicy::NotPermitted => None,

        AbstractionPolicy::Summarize => {
            Some(summarize_label(&field.field_name, &field.sensitivity))
        }

        AbstractionPolicy::RangeOnly => {
            Some(apply_range_only(&field.field_value, &field.field_name))
        }

        // Fail-safe: unknown policy -> omit (matches Python `return None`).
        AbstractionPolicy::Unknown(_) => None,
    }
}

fn summarize_label(field_name: &str, sensitivity: &Sensitivity) -> String {
    // Mirrors Python's sensitivity_labels dict + fallback exactly.
    match sensitivity {
        Sensitivity::General   => format!("a {}", field_name.replace('_', " ")),
        Sensitivity::Personal  => format!("personal {} information", field_name.replace('_', " ")),
        Sensitivity::Medical   => "medical information".to_string(),
        Sensitivity::Financial => "financial information".to_string(),
    }
}

fn apply_range_only(field_value: &str, field_name: &str) -> String {
    // Mirrors Python range_only branch exactly, including truncation-toward-zero
    // for negative values (int() in Python truncates toward zero, as does
    // f64 -> i64 cast in Rust).
    //
    // Python strip sequence:
    //   value.strip() -> remove leading/trailing whitespace
    //   .replace(",", "") -> remove thousands separators
    //   .replace("\u00a3", "") -> remove GBP symbol (checked first)
    //   .replace("$", "") -> remove USD symbol
    // Currency symbol detected on original value (before cleaning).

    let currency: &str = if field_value.contains('\u{00a3}') {
        "\u{00a3}"   // £
    } else if field_value.contains('$') {
        "$"
    } else {
        ""
    };

    let cleaned = field_value
        .trim()
        .replace(',', "")
        .replace('\u{00a3}', "")
        .replace('$', "");

    let numeric: f64 = match cleaned.parse::<f64>() {
        Ok(n)  => n,
        Err(_) => {
            // Non-numeric fallback: mirrors Python's except branch.
            return format!("a {}", field_name.replace('_', " "));
        }
    };

    // Replicate Python int() truncation-toward-zero using i64 cast.
    // Python: int((numeric * 0.80) / 5000) * 5000
    // Rust:   ((numeric * 0.80) / 5000.0) as i64 * 5000
    let low  = ((numeric * 0.80) / 5000.0) as i64 * 5000;
    let high = ((numeric * 1.20) / 5000.0 + 1.0) as i64 * 5000;

    if numeric >= 1000.0 {
        format!(
            "{currency}{low}k-{currency}{high}k",
            currency = currency,
            low      = low  / 1000,
            high     = high / 1000,
        )
    } else {
        format!("{low}-{high}")
    }
}
