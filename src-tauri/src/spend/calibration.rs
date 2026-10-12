// src-tauri/src/spend/calibration.rs
//
// Per-user, per-model calibration of the cost estimate from real response
// usage (items.id=711; decisions.id=875 point 6; spec Part 8e-8f; Chat-PM
// ruling on the plan's Q3 = A).
//
// WHAT IS CALIBRATED
//   input   the user's actual prompt tokens against QR's input estimate
//           (ceil(bytes / 4) x the model's token_factor). Stored as a
//           residual multiplier in `calibration.input_factor`.
//   output  hidden reasoning. Only when the provider reports
//           `reasoning_tokens`: observed overhead = completion tokens /
//           visible tokens (completion - reasoning), stored in
//           `calibration.output_factor` as observed / the row's
//           `output_overhead`, so the expected figure (`output_overhead` x
//           `output_factor`) reproduces what was observed.
//           ASSUMPTION, verified for ONE provider only (items.id=709 live
//           sample): the provider's completion token count includes its
//           reasoning tokens. If another provider counts them separately the
//           output factor is wrong for it. It feeds ONLY the informational
//           expected figure, never a cap check (see below), and items.id=726
//           verifies the other providers.
//
// SAFETY RULE FOR CAPS (Q3 = A). A factor below 1.0 would LOWER the input
// estimate the spend gate checks caps against, so a run of cheap prompts
// could let a later large one through a cap that should have stopped it.
// So the cap path and the worst-case figure use `max(1.0, factor)`
// (`cap_input_permille`); the informational expected figure uses the factor
// as stored in either direction.
//
// NAMED CONSTANTS (Chat-PM accepted): a factor applies only after
// MIN_SAMPLES samples of its kind, every sample and every applied factor is
// clamped to MIN_FACTOR_PERMILLE..=MAX_FACTOR_PERMILLE, and the running value
// is an exponentially weighted average with alpha = 1/EWMA_DEN. The first
// sample seeds the average.
//
// ARITHMETIC: integer permille only (1000 = 1.0), the same discipline as
// spend::money. The REAL column is written from decimal text by SQLite, never
// from a float here. A scan test keeps floats out of this file.
//
// WHICH CALLS COUNT: only a settled call with outcome `ok` and usage that the
// provider REPORTED. Truncated, budget-exhausted, failed and estimated-usage
// calls are ignored: their token counts do not describe a normal answer.

use chrono::{DateTime, Utc};
use sqlx::SqliteConnection;

use crate::persistence::provider_spend_store::{
    begin_immediate, commit, get_calibration_conn, rollback, upsert_calibration_conn,
    CalibrationText, CalibrationWrite, SpendStoreError,
};
use crate::spend::estimate::NEUTRAL_PERMILLE;
use crate::spend::money::{permille_ceil_text, scale_permille_ceil};

/// Samples of a kind needed before its factor is applied anywhere.
pub const MIN_SAMPLES: i64 = 3;
/// Smallest and largest factor (and sample), in permille: 0.25 to 4.0.
pub const MIN_FACTOR_PERMILLE: u64 = 250;
pub const MAX_FACTOR_PERMILLE: u64 = 4000;
/// The weight of a new sample is 1 / EWMA_DEN.
pub const EWMA_DEN: u64 = 4;

fn clamp(p: u64) -> u64 {
    p.clamp(MIN_FACTOR_PERMILLE, MAX_FACTOR_PERMILLE)
}

/// `(old x (DEN - 1) + sample) / DEN`, rounded to nearest.
pub fn ewma(old: u64, sample: u64) -> u64 {
    (old * (EWMA_DEN - 1) + sample + EWMA_DEN / 2) / EWMA_DEN
}

/// Permille as plain decimal text with three places: 1234 is `1.234`.
pub fn permille_text(p: u64) -> String {
    format!("{}.{:03}", p / 1000, p % 1000)
}

/// Calibration sample for the input estimate, in permille: actual prompt
/// tokens over QR's own estimate. `None` when either is zero.
pub fn input_sample(
    actual_prompt_tokens: u64,
    prompt_bytes: u64,
    token_factor_permille: u64,
) -> Option<u64> {
    let estimate = scale_permille_ceil(prompt_bytes.div_ceil(4), token_factor_permille);
    if actual_prompt_tokens == 0 || estimate == 0 {
        return None;
    }
    let p = u128::from(actual_prompt_tokens) * 1000 / u128::from(estimate);
    Some(clamp(u64::try_from(p).unwrap_or(u64::MAX)))
}

/// Calibration sample for the output overhead, in permille, relative to the
/// row's `output_overhead`. `None` unless the provider reported reasoning
/// tokens, with some visible output left.
pub fn output_sample(
    completion_tokens: u64,
    reasoning_tokens: Option<u64>,
    row_overhead_permille: u64,
) -> Option<u64> {
    let reasoning = reasoning_tokens?;
    let visible = completion_tokens.checked_sub(reasoning)?;
    if visible == 0 || row_overhead_permille == 0 {
        return None;
    }
    // observed overhead in permille = completion / visible
    let observed = u128::from(completion_tokens) * 1000 / u128::from(visible);
    let relative = observed * 1000 / u128::from(row_overhead_permille);
    Some(clamp(u64::try_from(relative).unwrap_or(u64::MAX)))
}

/// The factors in force, after the minimum-sample rule and the clamp.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Applied {
    pub input_permille: u64,
    pub output_permille: u64,
    pub input_applied: bool,
    pub output_applied: bool,
    pub input_samples: i64,
    pub output_samples: i64,
}

impl Applied {
    pub const NEUTRAL: Applied = Applied {
        input_permille: NEUTRAL_PERMILLE,
        output_permille: NEUTRAL_PERMILLE,
        input_applied: false,
        output_applied: false,
        input_samples: 0,
        output_samples: 0,
    };

    /// The input factor for anything a cap or the worst case depends on:
    /// never below 1.0 (Q3 = A).
    pub fn cap_input_permille(&self) -> u64 {
        self.input_permille.max(NEUTRAL_PERMILLE)
    }
}

/// Read a stored row as the factors in force.
pub fn applied_from(cal: Option<&CalibrationText>) -> Applied {
    let Some(c) = cal else {
        return Applied::NEUTRAL;
    };
    let usable = |text: &str, samples: i64| -> Option<u64> {
        if samples < MIN_SAMPLES {
            return None;
        }
        permille_ceil_text(text).map(clamp)
    };
    let input = usable(&c.input_factor, c.sample_count);
    let output = usable(&c.output_factor, c.output_sample_count);
    Applied {
        input_permille: input.unwrap_or(NEUTRAL_PERMILLE),
        output_permille: output.unwrap_or(NEUTRAL_PERMILLE),
        input_applied: input.is_some(),
        output_applied: output.is_some(),
        input_samples: c.sample_count,
        output_samples: c.output_sample_count,
    }
}

/// What one settled call tells calibration.
pub struct CallSample<'a> {
    pub provider_id: &'a str,
    pub model_id: &'a str,
    pub prompt_bytes: u64,
    pub token_factor_permille: u64,
    pub row_overhead_permille: u64,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub reasoning_tokens: Option<u64>,
}

/// Fold one call into the stored factors. One `BEGIN IMMEDIATE` transaction
/// (a concurrent call cannot lose an update). Nothing is written when the
/// call yields no usable sample.
pub async fn record_call(
    conn: &mut SqliteConnection,
    call: &CallSample<'_>,
    now: DateTime<Utc>,
) -> Result<bool, SpendStoreError> {
    let input = input_sample(
        call.tokens_in,
        call.prompt_bytes,
        call.token_factor_permille,
    );
    let output = output_sample(
        call.tokens_out,
        call.reasoning_tokens,
        call.row_overhead_permille,
    );
    if input.is_none() && output.is_none() {
        return Ok(false);
    }

    begin_immediate(conn).await?;
    let outcome = async {
        let existing = get_calibration_conn(conn, call.provider_id, call.model_id).await?;
        let (mut in_factor, mut out_factor, mut in_n, mut out_n) = match &existing {
            Some(c) => (
                permille_ceil_text(&c.input_factor).map_or(NEUTRAL_PERMILLE, clamp),
                permille_ceil_text(&c.output_factor).map_or(NEUTRAL_PERMILLE, clamp),
                c.sample_count,
                c.output_sample_count,
            ),
            None => (NEUTRAL_PERMILLE, NEUTRAL_PERMILLE, 0, 0),
        };
        if let Some(s) = input {
            in_factor = if in_n == 0 { s } else { ewma(in_factor, s) };
            in_n += 1;
        }
        if let Some(s) = output {
            out_factor = if out_n == 0 { s } else { ewma(out_factor, s) };
            out_n += 1;
        }
        upsert_calibration_conn(
            conn,
            &CalibrationWrite {
                provider_id: call.provider_id,
                model_id: call.model_id,
                input_factor: &permille_text(clamp(in_factor)),
                output_factor: &permille_text(clamp(out_factor)),
                sample_count: in_n,
                output_sample_count: out_n,
            },
            now,
        )
        .await
    }
    .await;
    match outcome {
        Ok(()) => {
            commit(conn).await?;
            Ok(true)
        }
        Err(e) => {
            rollback(conn).await;
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persistence::provider_spend_store::open;

    const KEY: &str = "0303030303030303030303030303030303030303030303030303030303030303";

    fn cal(i: &str, o: &str, n: i64, on: i64) -> CalibrationText {
        CalibrationText {
            input_factor: i.to_owned(),
            output_factor: o.to_owned(),
            sample_count: n,
            output_sample_count: on,
        }
    }

    #[test]
    fn permille_text_has_three_places() {
        assert_eq!(permille_text(1000), "1.000");
        assert_eq!(permille_text(1234), "1.234");
        assert_eq!(permille_text(250), "0.250");
        assert_eq!(permille_text(4000), "4.000");
        assert_eq!(permille_ceil_text(&permille_text(1234)), Some(1234));
    }

    #[test]
    fn ewma_weights_a_new_sample_by_a_quarter_and_rounds_to_nearest() {
        assert_eq!(ewma(1000, 1000), 1000);
        assert_eq!(ewma(1000, 2000), 1250);
        assert_eq!(ewma(2000, 1000), 1750);
        // 1000 x 3 + 1001 = 4001 / 4 = 1000.25 -> 1000
        assert_eq!(ewma(1000, 1001), 1000);
        // 1000 x 3 + 1002 = 4002 / 4 = 1000.5 -> 1001
        assert_eq!(ewma(1000, 1002), 1001);
    }

    #[test]
    fn input_sample_is_actual_over_the_qr_estimate_and_clamped() {
        // 4000 bytes -> 1000 tokens; the provider counted 1300.
        assert_eq!(input_sample(1300, 4000, 1000), Some(1300));
        // The token_factor is already in the estimate: 1000 x 1.3 = 1300.
        assert_eq!(input_sample(1300, 4000, 1300), Some(1000));
        assert_eq!(input_sample(100_000, 4000, 1000), Some(MAX_FACTOR_PERMILLE));
        assert_eq!(input_sample(1, 4000, 1000), Some(MIN_FACTOR_PERMILLE));
        assert_eq!(input_sample(0, 4000, 1000), None);
        assert_eq!(input_sample(10, 0, 1000), None);
    }

    #[test]
    fn output_sample_needs_reported_reasoning_and_is_relative_to_the_row() {
        // 300 completion tokens of which 100 reasoning: 200 visible, 1.5x.
        assert_eq!(output_sample(300, Some(100), 1000), Some(1500));
        // Against a row that already says 1.5, that is exactly neutral.
        assert_eq!(output_sample(300, Some(100), 1500), Some(1000));
        // Against a row that says 2.0 the model reasoned less than assumed.
        assert_eq!(output_sample(300, Some(100), 2000), Some(750));
        assert_eq!(output_sample(300, None, 1000), None);
        assert_eq!(
            output_sample(300, Some(300), 1000),
            None,
            "no visible output"
        );
        assert_eq!(
            output_sample(300, Some(400), 1000),
            None,
            "reasoning > total"
        );
        assert_eq!(output_sample(300, Some(0), 0), None);
    }

    #[test]
    fn a_factor_applies_only_after_the_minimum_samples_and_is_clamped() {
        assert_eq!(applied_from(None), Applied::NEUTRAL);
        let two = applied_from(Some(&cal("2.0", "1.5", 2, 2)));
        assert_eq!((two.input_permille, two.output_permille), (1000, 1000));
        assert!(!two.input_applied && !two.output_applied);
        let three = applied_from(Some(&cal("2.0", "1.5", 3, 3)));
        assert_eq!((three.input_permille, three.output_permille), (2000, 1500));
        assert!(three.input_applied && three.output_applied);
        // Counted separately.
        let mixed = applied_from(Some(&cal("2.0", "9.0", 5, 1)));
        assert_eq!((mixed.input_permille, mixed.output_permille), (2000, 1000));
        // An out-of-range stored value is clamped on read.
        let wild = applied_from(Some(&cal("50", "0.01", 9, 9)));
        assert_eq!(wild.input_permille, MAX_FACTOR_PERMILLE);
        assert_eq!(wild.output_permille, MIN_FACTOR_PERMILLE);
        // Unreadable text is neutral, not a guess.
        assert_eq!(
            applied_from(Some(&cal("abc", "1", 9, 9))).input_permille,
            1000
        );
    }

    #[test]
    fn the_cap_path_never_goes_below_one() {
        let low = applied_from(Some(&cal("0.5", "1", 9, 9)));
        assert_eq!(
            low.input_permille, 500,
            "the informational factor goes down"
        );
        assert_eq!(low.cap_input_permille(), 1000, "the cap path does not");
        let high = applied_from(Some(&cal("1.8", "1", 9, 9)));
        assert_eq!(high.cap_input_permille(), 1800);
        assert_eq!(Applied::NEUTRAL.cap_input_permille(), 1000);
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
        let conn = open("calib-user", KEY).await.expect("open keys db");
        f(conn).await;
        match saved {
            Some(v) => std::env::set_var("QR_DATA_ROOT", v),
            None => std::env::remove_var("QR_DATA_ROOT"),
        }
    }

    fn call(tokens_in: u64, tokens_out: u64, reasoning: Option<u64>) -> CallSample<'static> {
        CallSample {
            provider_id: "p",
            model_id: "m",
            prompt_bytes: 4000,
            token_factor_permille: 1000,
            row_overhead_permille: 2000,
            tokens_in,
            tokens_out,
            reasoning_tokens: reasoning,
        }
    }

    #[tokio::test]
    async fn the_first_sample_seeds_and_later_samples_average() {
        with_conn(|mut conn| async move {
            let now = Utc::now();
            assert!(record_call(&mut conn, &call(1500, 300, Some(100)), now)
                .await
                .unwrap());
            let c = get_calibration_conn(&mut conn, "p", "m")
                .await
                .unwrap()
                .unwrap();
            // input seeded at 1.5; output: 1.5x observed vs the row's 2.0 = 0.75
            assert_eq!(permille_ceil_text(&c.input_factor), Some(1500));
            assert_eq!(permille_ceil_text(&c.output_factor), Some(750));
            assert_eq!((c.sample_count, c.output_sample_count), (1, 1));

            record_call(&mut conn, &call(1000, 300, Some(100)), now)
                .await
                .unwrap();
            let c = get_calibration_conn(&mut conn, "p", "m")
                .await
                .unwrap()
                .unwrap();
            // (1500 x 3 + 1000) / 4 = 1375
            assert_eq!(permille_ceil_text(&c.input_factor), Some(1375));
            assert_eq!(c.sample_count, 2);
            // Not applied yet (2 < 3).
            assert!(!applied_from(Some(&c)).input_applied);
            record_call(&mut conn, &call(1000, 300, Some(100)), now)
                .await
                .unwrap();
            let c = get_calibration_conn(&mut conn, "p", "m")
                .await
                .unwrap()
                .unwrap();
            assert!(applied_from(Some(&c)).input_applied);
            assert!(applied_from(Some(&c)).output_applied);
        })
        .await;
    }

    #[tokio::test]
    async fn a_call_without_reasoning_tokens_updates_only_the_input_factor() {
        with_conn(|mut conn| async move {
            let now = Utc::now();
            record_call(&mut conn, &call(1200, 300, None), now)
                .await
                .unwrap();
            let c = get_calibration_conn(&mut conn, "p", "m")
                .await
                .unwrap()
                .unwrap();
            assert_eq!((c.sample_count, c.output_sample_count), (1, 0));
            assert_eq!(permille_ceil_text(&c.output_factor), Some(1000));
        })
        .await;
    }

    #[tokio::test]
    async fn a_call_with_no_usable_sample_writes_nothing() {
        with_conn(|mut conn| async move {
            let mut c = call(0, 0, None);
            c.prompt_bytes = 0;
            assert!(!record_call(&mut conn, &c, Utc::now()).await.unwrap());
            assert!(get_calibration_conn(&mut conn, "p", "m")
                .await
                .unwrap()
                .is_none());
        })
        .await;
    }

    #[test]
    fn source_has_no_floating_point() {
        let src = include_str!("calibration.rs");
        let code = src.split("#[cfg(test)]").next().unwrap();
        for needle in ["f64", "f32", "as f64", "powf", "powi"] {
            assert!(
                !code.contains(needle),
                "calibration.rs must hold no float ({needle})"
            );
        }
    }
}
