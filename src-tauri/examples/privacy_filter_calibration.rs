// src-tauri/examples/privacy_filter_calibration.rs
//
// One-off calibration run for items.id=36 / decisions.id=405 (Q2, tier routing).
// Runs the live Privacy Filter (privacy-filter.cpp FFI) against a corpus of
// representative QR field content and reports, per span:
//   - the category and confidence score the model actually returned
//   - the tier the CURRENT gate3.rs thresholds would assign it to
//   - whether the span was found at all (false negatives are the flagged
//     failure mode per D6-362/decisions.id=405 -- false positives are
//     acceptable, under-identification is not).
//
// The threshold constants below are copied from gate3.rs (not imported --
// they are private to that module). If gate3.rs's thresholds change, re-sync
// this file before re-running calibration.
//
// Usage (requires PRIVACY_FILTER_LIB_DIR set at BUILD time, same as the app):
//   export PRIVACY_FILTER_LIB_DIR=/home/kulaga/privacy-filter.cpp/build/release-portable
//   export LD_LIBRARY_PATH=$PRIVACY_FILTER_LIB_DIR/ggml/src:$LD_LIBRARY_PATH
//   cargo run --example privacy_filter_calibration --release

use quietrabbit_lib::conductor::privacy::privacy_filter::{self, PfEntityDecoded};

// -- Mirrors gate3.rs exactly (private consts there; duplicated here for this
// one-off report). See gate3.rs EASY_SCORE_THRESHOLD / MEDIUM_SCORE_THRESHOLD
// / EASY_TIER_CATEGORIES.
const EASY_SCORE_THRESHOLD: f32 = 0.90;
const MEDIUM_SCORE_THRESHOLD: f32 = 0.70;
const EASY_TIER_CATEGORIES: &[&str] = &["private_email", "private_phone", "account_number"];

#[derive(Clone, Copy)]
struct Case {
    id: &'static str,
    group: &'static str,
    text: &'static str,
    /// Category we expect the primary span to carry. None = negative control
    /// (no PII expected at all -- used to gauge false-positive rate, which
    /// decisions.id=405 says is an acceptable cost).
    expected_category: Option<&'static str>,
    /// Sensitivity severity gate3 would assign upstream (0=general .. 3=medical/financial).
    /// Included for context only -- calibration is about PF span/score behavior,
    /// not the severity-forced-High path (that path is untouched by PF accuracy
    /// by design, per decisions.id=405).
    content_sensitivity_severity: u8,
}

fn tier_for(score: f32, category: &str, severity: u8, target_tier: u8) -> &'static str {
    if target_tier >= 3 || severity >= 3 {
        return "High (forced: severity/tier)";
    }
    if score < MEDIUM_SCORE_THRESHOLD {
        return "High (low confidence)";
    }
    if score >= EASY_SCORE_THRESHOLD && EASY_TIER_CATEGORIES.contains(&category) {
        return "Easy";
    }
    "Medium"
}

fn corpus() -> Vec<Case> {
    vec![
        // -- Structural PII: expected Easy-tier categories (email/phone/account) --
        Case {
            id: "email-plain",
            group: "structural",
            text: "My email is jason.kulaga@gmail.com, feel free to reach out.",
            expected_category: Some("private_email"),
            content_sensitivity_severity: 1,
        },
        Case {
            id: "email-plus-addressed",
            group: "structural",
            text: "You can also try j.kulaga+work@protonmail.com for anything work related.",
            expected_category: Some("private_email"),
            content_sensitivity_severity: 1,
        },
        Case {
            id: "phone-dashed",
            group: "structural",
            text: "Call me at 555-234-8891 anytime after 6pm.",
            expected_category: Some("private_phone"),
            content_sensitivity_severity: 1,
        },
        Case {
            id: "phone-intl",
            group: "structural",
            text: "My number is +1 555 234 8891, text is fine too.",
            expected_category: Some("private_phone"),
            content_sensitivity_severity: 1,
        },
        Case {
            id: "account-number-bank",
            group: "structural",
            text: "My account number is 4521-8890-1123-4567 at the credit union.",
            expected_category: Some("account_number"),
            content_sensitivity_severity: 3,
        },
        Case {
            id: "account-number-routing",
            group: "structural",
            text: "Routing number 021000021, account 8834471290.",
            expected_category: Some("account_number"),
            content_sensitivity_severity: 3,
        },
        // -- Structural PII: high confidence expected, but NOT in EASY_TIER_CATEGORIES
        // (private_person / private_address default to Medium even at high score --
        // this is a deliberate gate3.rs design choice, calibration should confirm
        // the scores actually support "high confidence" for these categories).
        Case {
            id: "person-full-name",
            group: "structural-non-easy",
            text: "My name is Jason Kulaga and I live in Garuda.",
            expected_category: Some("private_person"),
            content_sensitivity_severity: 1,
        },
        Case {
            id: "person-intro",
            group: "structural-non-easy",
            text: "Hi, I'm Sarah Chen, nice to meet you.",
            expected_category: Some("private_person"),
            content_sensitivity_severity: 1,
        },
        Case {
            id: "address-full",
            group: "structural-non-easy",
            text: "I live at 742 Evergreen Terrace, Springfield.",
            expected_category: Some("private_address"),
            content_sensitivity_severity: 1,
        },
        Case {
            id: "address-street-only",
            group: "structural-non-easy",
            text: "My address is 12 Baker Street, Garuda City.",
            expected_category: Some("private_address"),
            content_sensitivity_severity: 1,
        },
        // -- Ambiguous names (classic NER traps -- common-word names, informal refs).
        // These probe for false negatives specifically -- the failure mode
        // decisions.id=405 calls credibility-destroying.
        Case {
            id: "name-common-word-rose",
            group: "ambiguous",
            text: "My sister Rose is visiting next week.",
            expected_category: Some("private_person"),
            content_sensitivity_severity: 1,
        },
        Case {
            id: "name-common-word-bill",
            group: "ambiguous",
            text: "Bill called me about the meeting yesterday.",
            expected_category: Some("private_person"),
            content_sensitivity_severity: 1,
        },
        Case {
            id: "name-embedded-family",
            group: "ambiguous",
            text: "My daughter Grace just started kindergarten.",
            expected_category: Some("private_person"),
            content_sensitivity_severity: 1,
        },
        Case {
            id: "name-nickname-only",
            group: "ambiguous",
            text: "Everyone just calls me Kool-Aid at the gym.",
            expected_category: Some("private_person"),
            content_sensitivity_severity: 1,
        },
        // -- Contextual / moderate confidence: financial, personal history, dietary/health.
        // decisions.id=405 Q2: these are NOT in the PF base taxonomy (8 categories).
        // Base model F1 on medical/financial domain terms is ~54% -- expected to be weak.
        // This is exactly what calibration needs to surface, not paper over.
        Case {
            id: "financial-income",
            group: "contextual",
            text: "My household income is around $85,000 a year.",
            expected_category: None,
            content_sensitivity_severity: 3,
        },
        Case {
            id: "financial-debt",
            group: "contextual",
            text: "I'm currently paying off about $12,000 in credit card debt.",
            expected_category: None,
            content_sensitivity_severity: 3,
        },
        Case {
            id: "medical-diagnosis",
            group: "contextual",
            text: "I was diagnosed with Type 2 diabetes back in 2019.",
            expected_category: None,
            content_sensitivity_severity: 3,
        },
        Case {
            id: "medical-medication",
            group: "contextual",
            text: "I take 500mg of Metformin twice daily.",
            expected_category: None,
            content_sensitivity_severity: 3,
        },
        Case {
            id: "dietary-health-context",
            group: "contextual",
            text: "I follow a low-carb diet because of my diabetes.",
            expected_category: None,
            content_sensitivity_severity: 3,
        },
        Case {
            id: "personal-history-divorce",
            group: "contextual",
            text: "I went through a difficult divorce last year.",
            expected_category: None,
            content_sensitivity_severity: 2,
        },
        // -- Secret category --
        Case {
            id: "secret-wifi-password",
            group: "secret",
            text: "My WiFi password is Sunflower88!",
            expected_category: Some("secret"),
            content_sensitivity_severity: 3,
        },
        Case {
            id: "secret-ssn",
            group: "secret",
            text: "My SSN is 123-45-6789.",
            expected_category: Some("secret"),
            content_sensitivity_severity: 3,
        },
        // -- URL / date --
        Case {
            id: "url-personal-site",
            group: "url-date",
            text: "Check out my portfolio at https://jasonkulaga.dev when you get a chance.",
            expected_category: Some("private_url"),
            content_sensitivity_severity: 1,
        },
        Case {
            id: "url-linkedin",
            group: "url-date",
            text: "My LinkedIn is linkedin.com/in/jasonkulaga.",
            expected_category: Some("private_url"),
            content_sensitivity_severity: 1,
        },
        Case {
            id: "date-birthday",
            group: "url-date",
            text: "I was born on March 3, 1990.",
            expected_category: Some("private_date"),
            content_sensitivity_severity: 1,
        },
        Case {
            id: "date-anniversary",
            group: "url-date",
            text: "Our anniversary is June 15th.",
            expected_category: Some("private_date"),
            content_sensitivity_severity: 1,
        },
        // -- Negative controls: no PII, measures false-positive rate only. --
        Case {
            id: "negative-weather",
            group: "negative-control",
            text: "The weather today is sunny with a light breeze.",
            expected_category: None,
            content_sensitivity_severity: 0,
        },
        Case {
            id: "negative-business",
            group: "negative-control",
            text: "Quarterly revenue increased by 12% year over year.",
            expected_category: None,
            content_sensitivity_severity: 0,
        },
        Case {
            id: "negative-generic-task",
            group: "negative-control",
            text: "Please summarize this document into three bullet points.",
            expected_category: None,
            content_sensitivity_severity: 0,
        },
    ]
}

fn main() {
    println!("# Privacy Filter Confidence Threshold Calibration\n");
    println!("Run against live privacy-filter.cpp model. items.id=36 / decisions.id=405.\n");

    if !privacy_filter::is_available() {
        eprintln!(
            "FATAL: privacy_filter::is_available() returned false. \
             PRIVACY_FILTER_LIB_DIR must be set at BUILD time and the GGUF model \
             must load successfully. Aborting -- no results to report."
        );
        std::process::exit(1);
    }
    println!("Privacy Filter: available and model loaded.\n");

    let cases = corpus();
    let mut missed = Vec::new();
    let mut wrong_category = Vec::new();
    let mut false_positives = Vec::new();

    println!(
        "| id | group | expected | detected spans (label:score) | tier (current thresholds) |"
    );
    println!("|---|---|---|---|---|");

    for c in &cases {
        let result = privacy_filter::run_classify_blocking(c.text, 0.0);
        let entities: Vec<PfEntityDecoded> = match result {
            Ok(e) => e,
            Err(e) => {
                eprintln!("[{}] PF call failed: {e}", c.id);
                continue;
            }
        };

        let detected_str = if entities.is_empty() {
            "(none)".to_string()
        } else {
            entities
                .iter()
                .map(|e| format!("{}:{:.3} \"{}\"", e.label, e.score, e.span_text))
                .collect::<Vec<_>>()
                .join("; ")
        };

        // Tier column: use the best-scoring entity matching the expected
        // category if we have one; otherwise the top-scoring entity found;
        // otherwise "n/a" (nothing detected).
        let target_tier_hint = if c.content_sensitivity_severity >= 3 {
            2
        } else {
            1
        };
        let tier_str = match c.expected_category {
            Some(expected) => {
                if let Some(best) = entities
                    .iter()
                    .filter(|e| e.label == expected)
                    .max_by(|a, b| a.score.partial_cmp(&b.score).unwrap())
                {
                    tier_for(
                        best.score,
                        &best.label,
                        c.content_sensitivity_severity,
                        target_tier_hint,
                    )
                    .to_string()
                } else {
                    "n/a (missed)".to_string()
                }
            }
            None => {
                if let Some(best) = entities
                    .iter()
                    .max_by(|a, b| a.score.partial_cmp(&b.score).unwrap())
                {
                    format!(
                        "{} (unexpected detection)",
                        tier_for(
                            best.score,
                            &best.label,
                            c.content_sensitivity_severity,
                            target_tier_hint
                        )
                    )
                } else {
                    "n/a (correctly none)".to_string()
                }
            }
        };

        println!(
            "| {} | {} | {} | {} | {} |",
            c.id,
            c.group,
            c.expected_category.unwrap_or("(none)"),
            detected_str,
            tier_str
        );

        match c.expected_category {
            Some(expected) => {
                let hit = entities.iter().any(|e| e.label == expected);
                if entities.is_empty() {
                    missed.push(c.id);
                } else if !hit {
                    wrong_category.push((
                        c.id,
                        entities.iter().map(|e| e.label.clone()).collect::<Vec<_>>(),
                    ));
                }
            }
            None => {
                if !entities.is_empty() {
                    false_positives.push(c.id);
                }
            }
        }
    }

    println!("\n## Summary\n");
    println!("Total cases: {}", cases.len());
    println!(
        "Cases with expected detection: {}",
        cases
            .iter()
            .filter(|c| c.expected_category.is_some())
            .count()
    );
    println!(
        "Negative-control cases: {}",
        cases
            .iter()
            .filter(|c| c.expected_category.is_none())
            .count()
    );
    println!(
        "\n**False negatives (expected span, PF found NOTHING) -- critical per decisions.id=405: {}**",
        missed.len()
    );
    for id in &missed {
        println!("  - {id}");
    }
    println!(
        "\n**Wrong-category detections (expected span found under a different label): {}**",
        wrong_category.len()
    );
    for (id, labels) in &wrong_category {
        println!("  - {id}: got {labels:?}");
    }
    println!(
        "\nFalse positives on negative controls (acceptable per decisions.id=405, informational only): {}",
        false_positives.len()
    );
    for id in &false_positives {
        println!("  - {id}");
    }
}
