// src-tauri/src/spend/money.rs
//
// Integer money arithmetic for the spend subsystem (items.id=710, scope item
// 7). Money is micro-USD (1 USD = 1,000,000) held in an `i64`; a price is
// micro-USD per MILLION tokens held in a `u64`, so the per-token cost
// `tokens * price / 1_000_000` is already in micro-USD.
//
// RULES:
//   - No floating point in any amount, cap, ledger sum or comparison.
//   - Every division rounds UP, so a figure is never under-counted.
//   - The one place a float is read at all is `permille_ceil`, which turns a
//     REAL multiplier from a catalog row (token_factor, output_overhead, a
//     calibration factor) into integer permille, rounded up, once, BEFORE any
//     money math. `source_scan_finds_no_float_in_ledger_and_cap_paths` (in
//     gate.rs's tests) keeps floats out of the paths that hold amounts.
//   - Decimal text from a price is converted by exact decimal parsing
//     (`parse_decimal_micros`), never through a float.

/// Micro-USD. 1 USD = 1_000_000.
pub type Micros = i64;

pub const MICROS_PER_USD: i64 = 1_000_000;

/// Largest multiplier (in permille) accepted from a catalog row: 1000x.
/// A bigger value is treated as bad data (the caller falls back to unknown).
pub const MAX_PERMILLE: u64 = 1_000_000;

/// ceil(n / d) for u128. `d` must be non-zero.
pub fn ceil_div_u128(n: u128, d: u128) -> u128 {
    debug_assert!(d != 0);
    n.div_ceil(d)
}

/// Cost in micro-USD of `tokens` at `price_micro_per_mtok` (micro-USD per
/// million tokens), rounded up and saturating at `i64::MAX`.
pub fn cost_micros(tokens: u64, price_micro_per_mtok: u64) -> Micros {
    let numerator = u128::from(tokens) * u128::from(price_micro_per_mtok);
    let c = ceil_div_u128(numerator, 1_000_000);
    i64::try_from(c).unwrap_or(i64::MAX)
}

/// Saturating add for amounts.
pub fn add(a: Micros, b: Micros) -> Micros {
    a.saturating_add(b)
}

/// Exact conversion of a decimal USD-per-million-tokens literal such as
/// `0.07` or `1.50` into micro-USD per million tokens (`70_000`, `1_500_000`).
///
/// Accepts only plain non-negative decimals: digits, at most one `.`, at most
/// six fractional digits, no sign, no exponent, no leading `.`/trailing `.`.
/// Anything else returns `None` (the price is then unknown, never rounded).
pub fn parse_decimal_micros(lexeme: &str) -> Option<u64> {
    let (int_part, frac_part) = match lexeme.split_once('.') {
        Some((i, f)) => (i, f),
        None => (lexeme, ""),
    };
    if int_part.is_empty() || !int_part.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    // "0.", ".5" and "1.2.3" are not valid JSON numbers either; reject them.
    if lexeme.contains('.') && frac_part.is_empty() {
        return None;
    }
    if frac_part.len() > 6 || !frac_part.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    // JSON forbids leading zeros ("01"), and a lone "0" is fine.
    if int_part.len() > 1 && int_part.starts_with('0') {
        return None;
    }
    let int_value: u64 = int_part.parse().ok()?;
    let mut frac_value: u64 = if frac_part.is_empty() {
        0
    } else {
        frac_part.parse().ok()?
    };
    for _ in frac_part.len()..6 {
        frac_value = frac_value.checked_mul(10)?;
    }
    int_value.checked_mul(1_000_000)?.checked_add(frac_value)
}

/// A REAL catalog multiplier (for example `token_factor` 1.3) as integer
/// permille, rounded UP. `None` for a non-finite, non-positive or absurdly
/// large value. This is the only place a float is read in the spend
/// subsystem, and it does no float arithmetic: it takes the float's shortest
/// decimal text (Rust's `Display` never uses an exponent) and rounds that text
/// up to three decimals, so 1.1 is 1100 permille, not 1101.
pub fn permille_ceil(x: f64) -> Option<u64> {
    if !x.is_finite() || x <= 0.0 {
        return None;
    }
    permille_ceil_text(&format!("{x}"))
}

/// The same conversion from decimal text such as `1.25` (what SQLite's
/// `CAST(real AS TEXT)` gives for a calibration factor). Plain decimals only:
/// no sign, no exponent. Extra fractional digits round the result up.
pub fn permille_ceil_text(text: &str) -> Option<u64> {
    let (int_part, frac_part) = text.split_once('.').unwrap_or((text, ""));
    if int_part.is_empty()
        || !int_part.bytes().all(|b| b.is_ascii_digit())
        || !frac_part.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let int_value: u64 = int_part.parse().ok()?;
    let mut head: String = frac_part.chars().take(3).collect();
    while head.len() < 3 {
        head.push('0');
    }
    let frac3: u64 = head.parse().ok()?;
    let rest_nonzero = frac_part.chars().skip(3).any(|c| c != '0');
    let mut permille = int_value.checked_mul(1000)?.checked_add(frac3)?;
    if rest_nonzero {
        permille = permille.checked_add(1)?;
    }
    if permille == 0 || permille > MAX_PERMILLE {
        None
    } else {
        Some(permille)
    }
}

/// ceil(tokens * permille / 1000).
pub fn scale_permille_ceil(tokens: u64, permille: u64) -> u64 {
    let c = ceil_div_u128(u128::from(tokens) * u128::from(permille), 1000);
    u64::try_from(c).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decimal_prices_parse_exactly() {
        assert_eq!(parse_decimal_micros("0.07"), Some(70_000));
        assert_eq!(parse_decimal_micros("0.30"), Some(300_000));
        assert_eq!(parse_decimal_micros("0.3"), Some(300_000));
        assert_eq!(parse_decimal_micros("1.50"), Some(1_500_000));
        assert_eq!(parse_decimal_micros("2.5"), Some(2_500_000));
        assert_eq!(parse_decimal_micros("0"), Some(0));
        assert_eq!(parse_decimal_micros("0.000001"), Some(1));
        assert_eq!(parse_decimal_micros("15"), Some(15_000_000));
        assert_eq!(parse_decimal_micros("0.035"), Some(35_000));
    }

    #[test]
    fn unrepresentable_decimals_are_rejected_not_rounded() {
        for bad in [
            "",
            "-1",
            "-0.5",
            "+1",
            "1e-7",
            "1E3",
            "0.0000001",
            "0.1234567",
            ".5",
            "5.",
            "1.2.3",
            "01",
            "0x10",
            "NaN",
            "inf",
            " 1",
            "1 ",
            "1,5",
        ] {
            assert_eq!(parse_decimal_micros(bad), None, "{bad:?} must be rejected");
        }
        // Overflow of the u64 micro range.
        assert_eq!(parse_decimal_micros("99999999999999999999"), None);
    }

    #[test]
    fn cost_rounds_up_and_never_under_counts() {
        // 1 token at 70_000 micro/Mtok is 0.07 micro-USD -> 1.
        assert_eq!(cost_micros(1, 70_000), 1);
        // 1_000_000 tokens at 70_000 is exactly 70_000 micro-USD.
        assert_eq!(cost_micros(1_000_000, 70_000), 70_000);
        // 1_000_001 tokens rounds up by one.
        assert_eq!(cost_micros(1_000_001, 70_000), 70_001);
        // Free is free.
        assert_eq!(cost_micros(123_456, 0), 0);
        assert_eq!(cost_micros(0, 5_000_000), 0);
    }

    #[test]
    fn cost_saturates_instead_of_overflowing() {
        assert_eq!(cost_micros(u64::MAX, u64::MAX), i64::MAX);
        assert_eq!(add(i64::MAX, 5), i64::MAX);
    }

    #[test]
    fn multipliers_round_up_to_permille() {
        assert_eq!(permille_ceil(1.0), Some(1000));
        assert_eq!(permille_ceil(1.3), Some(1300));
        assert_eq!(permille_ceil(2.0), Some(2000));
        // 1.0004 must not round down to 1.000.
        assert_eq!(permille_ceil(1.0004), Some(1001));
        // Decimal text, not float arithmetic: 1.1 is exactly 1100.
        assert_eq!(permille_ceil(1.1), Some(1100));
        assert_eq!(permille_ceil(0.001), Some(1));
        assert_eq!(permille_ceil(0.0001), Some(1));
        assert_eq!(permille_ceil(1.2345678901), Some(1235));
        assert_eq!(permille_ceil(0.0), None);
        assert_eq!(permille_ceil(-1.0), None);
        assert_eq!(permille_ceil(f64::NAN), None);
        assert_eq!(permille_ceil(f64::INFINITY), None);
        assert_eq!(permille_ceil(1.0e9), None);
        assert_eq!(permille_ceil(1.0e300), None);
        // Text form, as SQLite gives a REAL column.
        assert_eq!(permille_ceil_text("1.25"), Some(1250));
        assert_eq!(permille_ceil_text("1.0"), Some(1000));
        assert_eq!(permille_ceil_text("2"), Some(2000));
        assert_eq!(permille_ceil_text("1.0e-05"), None);
        assert_eq!(permille_ceil_text("-1"), None);
        assert_eq!(permille_ceil_text(""), None);
        assert_eq!(permille_ceil_text("0"), None);
    }

    #[test]
    fn scaling_rounds_up() {
        assert_eq!(scale_permille_ceil(1000, 1000), 1000);
        assert_eq!(scale_permille_ceil(1000, 1300), 1300);
        assert_eq!(scale_permille_ceil(1, 1001), 2);
        assert_eq!(scale_permille_ceil(3, 1000), 3);
    }
}
