// src-tauri/src/spend/estimate.rs
//
// The pre-flight estimate (items.id=710, decisions.id=875 point 4; Part 8e):
// worst case = the call's output token limit x the output price + the
// estimated input x the input price. All integer, all rounded up.
//
// WORST CASE vs EXPECTED:
//   - The worst case is what the pre-flight checks against caps. It assumes
//     the model emits the full output token limit. `output_overhead` does NOT
//     raise it: hidden reasoning tokens are counted inside the provider's
//     output limit (observed for one provider live in items.id=709; assumed
//     for the rest, unverified, items.id=726), so the limit is already the
//     ceiling.
//   - The expected figure exists only when the caller supplies an expected
//     visible output size. It applies output_overhead and the user's output
//     calibration factor. There is no invented default for expected output.
//
// INPUT ESTIMATE: ceil(bytes / 4) tokens, scaled by the model's token_factor
// and the user's input calibration factor (both converted to integer permille,
// rounded up, by the caller). This is a heuristic like the context-window
// check's, but it always rounds up and does no float arithmetic.

use crate::spend::money::{add, cost_micros, scale_permille_ceil, Micros};
use crate::spend::price::Price;

/// Margin applied to the estimated input when choosing a prompt-length price
/// tier before the call (the real prompt size is only known afterwards).
pub const TIER_MARGIN_PERMILLE: u64 = 1250;

/// Neutral multiplier: 1.000.
pub const NEUTRAL_PERMILLE: u64 = 1000;

/// Estimated input tokens for a prompt of `prompt_bytes` bytes.
pub fn input_tokens_estimate(
    prompt_bytes: u64,
    token_factor_permille: u64,
    calibration_permille: u64,
) -> u64 {
    let base = prompt_bytes.div_ceil(4);
    let scaled = scale_permille_ceil(base, token_factor_permille);
    scale_permille_ceil(scaled, calibration_permille)
}

/// The figures a pre-flight needs, all in integer units.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Estimate {
    pub input_tokens: u64,
    pub output_limit_tokens: u64,
    pub input_micros: Micros,
    pub worst_output_micros: Micros,
    pub worst_case_micros: Micros,
}

/// Worst-case estimate for a call. The tier is chosen from the estimated
/// input plus the safety margin, so a prompt near a tier boundary is priced at
/// the dearer tier.
pub fn worst_case(price: &Price, input_tokens: u64, output_limit_tokens: u64) -> Estimate {
    let tier_probe = scale_permille_ceil(input_tokens, TIER_MARGIN_PERMILLE);
    let (in_rate, out_rate) = price.rates_for(tier_probe);
    // Never below the rate the unmargined estimate would pick.
    let (in_rate2, out_rate2) = price.rates_for(input_tokens);
    let in_rate = in_rate.max(in_rate2);
    let out_rate = out_rate.max(out_rate2);

    let input_micros = cost_micros(input_tokens, in_rate);
    let worst_output_micros = cost_micros(output_limit_tokens, out_rate);
    Estimate {
        input_tokens,
        output_limit_tokens,
        input_micros,
        worst_output_micros,
        worst_case_micros: add(input_micros, worst_output_micros),
    }
}

/// Expected cost when the caller supplies an expected visible output size.
/// Applies `output_overhead` and the output calibration factor (permille).
pub fn expected_micros(
    price: &Price,
    input_tokens: u64,
    expected_output_tokens: u64,
    output_overhead_permille: u64,
    output_calibration_permille: u64,
) -> Micros {
    let (in_rate, out_rate) = price.rates_for(input_tokens);
    let out_tokens = scale_permille_ceil(
        scale_permille_ceil(expected_output_tokens, output_overhead_permille),
        output_calibration_permille,
    );
    add(
        cost_micros(input_tokens, in_rate),
        cost_micros(out_tokens, out_rate),
    )
}

/// Actual cost of a settled call from reported usage and the same price.
/// The tier comes from the REAL prompt size.
pub fn actual_micros(price: &Price, prompt_tokens: u64, completion_tokens: u64) -> Micros {
    let (in_rate, out_rate) = price.rates_for(prompt_tokens);
    add(
        cost_micros(prompt_tokens, in_rate),
        cost_micros(completion_tokens, out_rate),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spend::price::parse_pricing;

    fn price(raw: &str) -> Price {
        parse_pricing(raw, None).expect("fixture price")
    }

    const FLAT: &str = r#"{"input_per_mtok":0.50,"output_per_mtok":2.00,"source":"curated"}"#;
    const TIERED: &str = r#"{"input_per_mtok":0.10,"output_per_mtok":0.50,"tiers":[
        {"prompt_tokens_up_to":1000,"input_per_mtok":0.10,"output_per_mtok":0.50},
        {"prompt_tokens_over":1000,"input_per_mtok":1.00,"output_per_mtok":5.00}],
        "source":"curated"}"#;

    #[test]
    fn input_estimate_rounds_up_and_scales() {
        assert_eq!(input_tokens_estimate(0, 1000, 1000), 0);
        assert_eq!(input_tokens_estimate(1, 1000, 1000), 1);
        assert_eq!(input_tokens_estimate(4, 1000, 1000), 1);
        assert_eq!(input_tokens_estimate(5, 1000, 1000), 2);
        assert_eq!(input_tokens_estimate(4000, 1000, 1000), 1000);
        // A 1.3 tokenizer factor.
        assert_eq!(input_tokens_estimate(4000, 1300, 1000), 1300);
        // Calibration compounds.
        assert_eq!(input_tokens_estimate(4000, 1300, 2000), 2600);
    }

    #[test]
    fn worst_case_is_input_plus_the_full_output_limit() {
        let e = worst_case(&price(FLAT), 1_000, 4_096);
        // 1_000 tokens x 500_000 / 1e6 = 500; 4_096 x 2_000_000 / 1e6 = 8_192.
        assert_eq!(e.input_micros, 500);
        assert_eq!(e.worst_output_micros, 8_192);
        assert_eq!(e.worst_case_micros, 8_692);
    }

    #[test]
    fn worst_case_rounds_up_to_at_least_one_micro() {
        let e = worst_case(&price(FLAT), 1, 1);
        assert_eq!(e.input_micros, 1);
        assert_eq!(e.worst_output_micros, 2);
    }

    #[test]
    fn a_free_price_estimates_zero() {
        let free = price(r#"{"input_per_mtok":0,"output_per_mtok":0}"#);
        let e = worst_case(&free, 50_000, 4_096);
        assert_eq!(e.worst_case_micros, 0);
    }

    #[test]
    fn near_a_tier_boundary_the_dearer_tier_is_assumed() {
        // 900 tokens is under the 1000 boundary, but with the 25 percent
        // margin the probe is 1125, so the dearer tier prices the worst case.
        let e = worst_case(&price(TIERED), 900, 100);
        assert_eq!(e.input_micros, 900); // 900 x 1_000_000 / 1e6
        assert_eq!(e.worst_output_micros, 500); // 100 x 5_000_000 / 1e6
                                                // Well under the boundary stays on the cheap tier.
        let low = worst_case(&price(TIERED), 100, 100);
        assert_eq!(low.input_micros, 10); // 100 x 100_000 / 1e6
        assert_eq!(low.worst_output_micros, 50);
    }

    #[test]
    fn actual_cost_uses_the_real_prompt_size_for_the_tier() {
        let p = price(TIERED);
        assert_eq!(actual_micros(&p, 1_000, 10), 100 + 5);
        assert_eq!(actual_micros(&p, 1_001, 10), 1_001 + 50);
    }

    #[test]
    fn expected_applies_overhead_and_calibration_only_there() {
        let p = price(FLAT);
        let plain = expected_micros(&p, 1_000, 1_000, 1000, 1000);
        // 500 + 2_000
        assert_eq!(plain, 2_500);
        let with_overhead = expected_micros(&p, 1_000, 1_000, 2000, 1000);
        // 500 + 2_000_000 x 2 / 1e6 x 1000 -> 2000 tokens x 2 = 4_000
        assert_eq!(with_overhead, 4_500);
        // The worst case is unchanged by overhead.
        assert_eq!(worst_case(&p, 1_000, 1_000).worst_case_micros, 2_500);
    }
}
