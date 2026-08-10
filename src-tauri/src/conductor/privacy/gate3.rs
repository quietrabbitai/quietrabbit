// src-tauri/src/conductor/privacy/gate3.rs
//
// PG_GATE_3: cross-tier content promotion guardian with Privacy Filter integration.
//
// Gate ordering is load-bearing — do NOT reorder:
//   1. Tier ceiling block (target_tier > space_max_permitted_tier) — unchanged
//   2a. Privacy Filter path (compiled in + app_handle present):
//       - spawn_blocking FFI call with 10s timeout (non-negotiable)
//       - Timeout           → gate_timeout disclosure event + blocked result
//       - spawn_blocking panic → same as timeout (conservative)
//       - PF error          → fall back to legacy sensitivity block
//       - Zero spans, severity < 3 AND target_tier < 3 → gate3_pf_no_spans + approved (no modal)
//       - Zero spans, severity >= 3 OR target_tier >= 3 → still routed to the
//         consent gate (High tier, empty spans list) — see D6-362/decisions.id=405
//         note below. NOT auto-approved.
//       - Non-zero spans    → gate3_consent_pending written THEN consent_request emit
//   2b. Legacy sensitivity block fallback (no PF or no handle — dev/test builds):
//       - severity >= 3 AND target_tier >= 2 → gate3_sensitivity_block + blocked
//       - Otherwise → gate3_promotion_approved + approved
//
// D6-362: PG_GATE_3 fires unconditionally when sensitivity_ceiling > 0 AND
// content is about to cross a tier boundary. Fallback path preserves existing
// golden-vector behavior for builds where Privacy Filter library is not compiled in.
//
// decisions.id=405 (Q2): High tier always applies to Medical/Financial content
// and to any content where Privacy Filter confidence is low, regardless of PF
// confidence. Zero PF detections is the extreme case of low confidence — it
// must NOT be treated as "nothing to review." Content whose sensitivity comes
// from context outside PF's base taxonomy (financial figures, medical history)
// routinely returns zero spans; auto-approving on span count alone bypasses
// the severity-forced-High rule for exactly the content it's meant to protect.
// See items.id=36 / PRIVACY_FILTER_THRESHOLD_CALIBRATION.md finding #5.
//
// Write-before-surface invariant: disclosure_log write MUST precede any emit()
// call. If the log write fails (fatal DisclosureLogWriteError), the frontend
// must not have received the event.

use std::time::Duration;

use indexmap::IndexMap;
use tauri::Emitter;
use uuid::Uuid;

use super::{
    errors::DisclosureLogWriteError,
    logger::{DisclosureLogEntry, DisclosureLogger},
    privacy_filter::{self, PfEntityDecoded},
    types::{ConsentRequestPayload, ConsentSpanItem, Gate3Result, ReviewTier},
};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Maximum time allowed for the Privacy Filter FFI call inside spawn_blocking.
/// IPC flag: timeout → gate_timeout event written to disclosure_log (D6-362).
const PF_TIMEOUT_SECS: u64 = 10;

/// Minimum confidence score for Easy tier. ALL spans must meet this threshold
/// AND be in EASY_TIER_CATEGORIES.
const EASY_SCORE_THRESHOLD: f32 = 0.90;

/// Minimum confidence for Medium tier. Any span below this forces High.
/// Errs toward High per D6-362.
const MEDIUM_SCORE_THRESHOLD: f32 = 0.70;

/// Categories that qualify for Easy tier when all spans exceed EASY_SCORE_THRESHOLD.
/// Contextual categories (private_date, private_url, secret) default to Medium
/// even at high confidence.
const EASY_TIER_CATEGORIES: &[&str] = &["private_email", "private_phone", "account_number"];

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)] // Explicit architecture boundary; see D6-342/D6-346.
pub async fn gate3<L: DisclosureLogger>(
    logger: &L,
    step_id: &str,
    focus_run_id: &str,
    focus_name: &str,
    content_key: &str,
    content_text: &str,
    content_sensitivity_severity: u8,
    target_tier: u8,
    space_max_permitted_tier: u8,
    execution_tier: u8,
    app_handle: Option<&tauri::AppHandle<tauri::Wry>>,
) -> Result<Gate3Result, DisclosureLogWriteError> {
    // Check 1: tier ceiling block — fires first, before any other check.
    if target_tier > space_max_permitted_tier {
        logger
            .write(DisclosureLogEntry {
                step_id: step_id.to_string(),
                focus_run_id: focus_run_id.to_string(),
                execution_tier,
                abstraction_tier: None,
                provider: None,
                fields_shared: vec![],
                fields_abstracted: IndexMap::new(),
                fields_withheld: vec![content_key.to_string()],
                override_declined: true,
                event_type: "gate3_tier_ceiling_block".to_string(),
            })
            .await?;

        return Ok(Gate3Result {
            blocked: true,
            plain_language: Some(
                "This content can't be shared with a higher-tier service \
                 from this Focus. [Change Focus settings] [Use local only]"
                    .to_string(),
            ),
            ..Gate3Result::default()
        });
    }

    // Check 2a: Privacy Filter path.
    // Requires PF compiled in, model loaded, and AppHandle for emit.
    // if let: idiomatic; avoids is_some() + unwrap() pattern.
    if content_sensitivity_severity > 0 && privacy_filter::is_available() {
        if let Some(handle) = app_handle {
            return gate3_with_pf(
                logger,
                step_id,
                focus_run_id,
                focus_name,
                content_key,
                content_text,
                content_sensitivity_severity,
                target_tier,
                execution_tier,
                handle,
            )
            .await;
        }
    }

    // Check 2b: Legacy sensitivity block fallback.
    // Active when PF is not compiled in (dev/test) or no app_handle (unit tests).
    // Preserves gate3 golden-vector behavior.
    if content_sensitivity_severity >= 3 && target_tier >= 2 {
        logger
            .write(DisclosureLogEntry {
                step_id: step_id.to_string(),
                focus_run_id: focus_run_id.to_string(),
                execution_tier,
                abstraction_tier: None,
                provider: None,
                fields_shared: vec![],
                fields_abstracted: IndexMap::new(),
                fields_withheld: vec![content_key.to_string()],
                override_declined: true,
                event_type: "gate3_sensitivity_block".to_string(),
            })
            .await?;

        return Ok(Gate3Result {
            blocked: true,
            plain_language: Some(
                "This content contains medical or financial information \
                 and can't be shared with external services. \
                 [Use local only] [Get help]"
                    .to_string(),
            ),
            ..Gate3Result::default()
        });
    }

    // Approved.
    logger
        .write(DisclosureLogEntry {
            step_id: step_id.to_string(),
            focus_run_id: focus_run_id.to_string(),
            execution_tier,
            abstraction_tier: None,
            provider: None,
            fields_shared: vec![content_key.to_string()],
            fields_abstracted: IndexMap::new(),
            fields_withheld: vec![],
            override_declined: false,
            event_type: "gate3_promotion_approved".to_string(),
        })
        .await?;

    Ok(Gate3Result {
        approved: true,
        ..Gate3Result::default()
    })
}

// ---------------------------------------------------------------------------
// Privacy Filter path
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
async fn gate3_with_pf<L: DisclosureLogger>(
    logger: &L,
    step_id: &str,
    focus_run_id: &str,
    focus_name: &str,
    content_key: &str,
    content_text: &str,
    content_sensitivity_severity: u8,
    target_tier: u8,
    execution_tier: u8,
    handle: &tauri::AppHandle<tauri::Wry>,
) -> Result<Gate3Result, DisclosureLogWriteError> {
    let text = content_text.to_owned();

    // spawn_blocking: FFI call is synchronous C library — must not block async executor.
    let pf_task =
        tokio::task::spawn_blocking(move || privacy_filter::run_classify_blocking(&text, 0.0));

    let pf_outcome = tokio::time::timeout(Duration::from_secs(PF_TIMEOUT_SECS), pf_task).await;

    let entities: Vec<PfEntityDecoded> = match pf_outcome {
        // Timeout: write gate_timeout to disclosure_log (IPC flag — distinct event type).
        Err(_elapsed) => {
            log::warn!("gate3: Privacy Filter timed out after {PF_TIMEOUT_SECS}s");
            logger
                .write(DisclosureLogEntry {
                    step_id: step_id.to_string(),
                    focus_run_id: focus_run_id.to_string(),
                    execution_tier,
                    abstraction_tier: None,
                    provider: None,
                    fields_shared: vec![],
                    fields_abstracted: IndexMap::new(),
                    fields_withheld: vec![content_key.to_string()],
                    override_declined: true,
                    event_type: "gate_timeout".to_string(),
                })
                .await?;
            let _ = handle.emit(
                "gate_timeout",
                serde_json::json!({ "focus_run_id": focus_run_id }),
            );
            return Ok(Gate3Result {
                blocked: true,
                timeout: true,
                plain_language: Some(
                    "Privacy review timed out. Content blocked. [Try again]".to_string(),
                ),
                ..Gate3Result::default()
            });
        }

        // spawn_blocking panicked: treat conservatively — same path as timeout.
        Ok(Err(join_err)) => {
            log::error!("gate3: Privacy Filter task panicked: {join_err}");
            logger
                .write(DisclosureLogEntry {
                    step_id: step_id.to_string(),
                    focus_run_id: focus_run_id.to_string(),
                    execution_tier,
                    abstraction_tier: None,
                    provider: None,
                    fields_shared: vec![],
                    fields_abstracted: IndexMap::new(),
                    fields_withheld: vec![content_key.to_string()],
                    override_declined: true,
                    event_type: "gate_timeout".to_string(),
                })
                .await?;
            let _ = handle.emit(
                "gate_timeout",
                serde_json::json!({ "focus_run_id": focus_run_id }),
            );
            return Ok(Gate3Result {
                blocked: true,
                timeout: true,
                plain_language: Some(
                    "Privacy review failed. Content blocked. [Try again]".to_string(),
                ),
                ..Gate3Result::default()
            });
        }

        // PF returned an error (model unavailable, bad GGUF, etc.): fall back to
        // sensitivity block. Logs at warn — does not treat as F_SYSTEM.
        Ok(Ok(Err(pf_err))) => {
            log::warn!(
                "gate3: Privacy Filter returned error — falling back to sensitivity block: \
                 {pf_err}"
            );
            if content_sensitivity_severity >= 3 && target_tier >= 2 {
                logger
                    .write(DisclosureLogEntry {
                        step_id: step_id.to_string(),
                        focus_run_id: focus_run_id.to_string(),
                        execution_tier,
                        abstraction_tier: None,
                        provider: None,
                        fields_shared: vec![],
                        fields_abstracted: IndexMap::new(),
                        fields_withheld: vec![content_key.to_string()],
                        override_declined: true,
                        event_type: "gate3_sensitivity_block".to_string(),
                    })
                    .await?;
                return Ok(Gate3Result {
                    blocked: true,
                    plain_language: Some(
                        "This content contains medical or financial information \
                         and can't be shared with external services. \
                         [Use local only] [Get help]"
                            .to_string(),
                    ),
                    ..Gate3Result::default()
                });
            }
            logger
                .write(DisclosureLogEntry {
                    step_id: step_id.to_string(),
                    focus_run_id: focus_run_id.to_string(),
                    execution_tier,
                    abstraction_tier: None,
                    provider: None,
                    fields_shared: vec![content_key.to_string()],
                    fields_abstracted: IndexMap::new(),
                    fields_withheld: vec![],
                    override_declined: false,
                    event_type: "gate3_promotion_approved".to_string(),
                })
                .await?;
            return Ok(Gate3Result {
                approved: true,
                ..Gate3Result::default()
            });
        }

        // PF succeeded: process the entity list.
        Ok(Ok(Ok(entities))) => entities,
    };

    // Zero spans, NOT severity/tier-forced: PF found nothing identifiable and
    // there's no independent reason to force review — approve directly.
    // D6-362: gate still fired (field-tracking trigger), nothing to surface to user.
    //
    // FIX (items.id=36): this branch previously fired on `entities.is_empty()`
    // alone, auto-approving BEFORE assign_review_tier's severity/target_tier
    // check ever ran. That silently shipped severity>=3 content (e.g. financial
    // figures outside PF's base taxonomy) with no consent modal at all —
    // contradicting decisions.id=405 Q2's "Medical/Financial always High
    // regardless of PF confidence" rule. The severity/target_tier guard below
    // must stay in sync with assign_review_tier's forced-High condition.
    if entities.is_empty()
        && zero_spans_safe_to_auto_approve(content_sensitivity_severity, target_tier)
    {
        logger
            .write(DisclosureLogEntry {
                step_id: step_id.to_string(),
                focus_run_id: focus_run_id.to_string(),
                execution_tier,
                abstraction_tier: None,
                provider: None,
                fields_shared: vec![content_key.to_string()],
                fields_abstracted: IndexMap::new(),
                fields_withheld: vec![],
                override_declined: false,
                event_type: "gate3_pf_no_spans".to_string(),
            })
            .await?;
        return Ok(Gate3Result {
            approved: true,
            ..Gate3Result::default()
        });
    }

    // Non-zero spans, OR zero spans that severity/target_tier force to High:
    // build consent payload (spans list may be empty in the forced-High/
    // no-detection case — frontend must handle an empty spans list by still
    // surfacing the High-tier consent gate, not by treating it as nothing to
    // show), write audit record, THEN emit event.
    // Write-before-surface invariant: log write must precede emit() — if the write
    // fails (fatal DisclosureLogWriteError), the frontend must not receive the event.
    let spans = build_consent_spans(&entities);
    let review_tier = assign_review_tier(&entities, content_sensitivity_severity, target_tier);
    let no_spans_forced_high = spans.is_empty();

    let payload = ConsentRequestPayload {
        focus_run_id: focus_run_id.to_owned(),
        focus_name: focus_name.to_owned(),
        review_tier,
        spans,
    };

    logger
        .write(DisclosureLogEntry {
            step_id: step_id.to_string(),
            focus_run_id: focus_run_id.to_string(),
            execution_tier,
            abstraction_tier: None,
            provider: None,
            fields_shared: vec![],
            fields_abstracted: IndexMap::new(),
            fields_withheld: vec![content_key.to_string()],
            override_declined: false,
            event_type: if no_spans_forced_high {
                "gate3_pf_no_spans_forced_review".to_string()
            } else {
                "gate3_consent_pending".to_string()
            },
        })
        .await?;

    let _ = handle.emit("consent_request", &payload);

    Ok(Gate3Result {
        pending_consent: true,
        ..Gate3Result::default()
    })
}

// ---------------------------------------------------------------------------
// Span assembly helpers
// ---------------------------------------------------------------------------

fn build_consent_spans(entities: &[PfEntityDecoded]) -> Vec<ConsentSpanItem> {
    entities
        .iter()
        .map(|e| ConsentSpanItem {
            span_id: Uuid::new_v4().to_string(),
            category: e.label.clone(),
            user_label: taxonomy_label(&e.label),
            original_text: e.span_text.clone(),
            suggestion: generalization_suggestion(&e.label),
            start_byte: e.start_byte,
            end_byte: e.end_byte,
            score: e.score,
        })
        .collect()
}

/// True only when PF returning zero spans is safe to auto-approve without a
/// consent gate: severity and target_tier must both be below the High-forcing
/// thresholds used by `assign_review_tier`. Kept in sync with that function's
/// `target_tier >= 3 || content_sensitivity_severity >= 3` condition — this is
/// the entities-independent half of the same rule (items.id=36).
fn zero_spans_safe_to_auto_approve(content_sensitivity_severity: u8, target_tier: u8) -> bool {
    !(content_sensitivity_severity >= 3 || target_tier >= 3)
}

fn assign_review_tier(
    entities: &[PfEntityDecoded],
    content_sensitivity_severity: u8,
    target_tier: u8,
) -> ReviewTier {
    // High: Medical/Financial context, Tier 3 target, or any low-confidence span.
    if target_tier >= 3 || content_sensitivity_severity >= 3 {
        return ReviewTier::High;
    }
    for e in entities {
        if e.score < MEDIUM_SCORE_THRESHOLD {
            return ReviewTier::High;
        }
    }
    // Easy: all spans high-confidence AND structural PII categories.
    if entities.iter().all(|e| {
        e.score >= EASY_SCORE_THRESHOLD && EASY_TIER_CATEGORIES.contains(&e.label.as_str())
    }) {
        return ReviewTier::Easy;
    }
    ReviewTier::Medium
}

/// Human-readable display label for a Privacy Filter category. Must match
/// PRIVACY_GUARDIAN_GATE_SPEC.md's taxonomy table verbatim — that spec is
/// locked/authoritative; this function conforms to it, not the reverse.
fn taxonomy_label(category: &str) -> String {
    match category {
        "private_person" => "Name or identity",
        "private_address" => "Address or location",
        "private_email" => "Email address",
        "private_phone" => "Phone number",
        "private_url" => "Personal web address",
        "private_date" => "Personal date",
        "account_number" => "Account number",
        "secret" => "Sensitive value",
        _ => "Sensitive information",
    }
    .to_owned()
}

/// Pre-populated generalization suggestion for a category.
/// None → no rule matches → frontend renders editable placeholder (IPC flag 3).
fn generalization_suggestion(category: &str) -> Option<String> {
    let s = match category {
        "private_person" => "[person]",
        "private_address" => "[address]",
        "private_email" => "[email address]",
        "private_phone" => "[phone number]",
        "private_url" => "[web address]",
        "private_date" => "[date]",
        "account_number" => "[account number]",
        "secret" => "[sensitive value]",
        _ => return None,
    };
    Some(s.to_owned())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conductor::privacy::privacy_filter::PfEntityDecoded;

    fn entity(score: f32, label: &str) -> PfEntityDecoded {
        PfEntityDecoded {
            start_byte: 0,
            end_byte: 5,
            score,
            label: label.to_owned(),
            span_text: "test".to_owned(),
        }
    }

    // -- zero_spans_safe_to_auto_approve -------------------------------------
    // items.id=36: zero-span PF results must NOT bypass the severity/target_tier
    // forced-High rule. Repro case from calibration testing: financial content
    // ("household income is around $85,000", "$12,000 in credit card debt")
    // tagged content_sensitivity_severity=3 returns zero PF spans because
    // financial/medical context is outside PF's base taxonomy (decisions.id=405
    // Q2) -- that must still force a High-tier consent gate, not auto-approve.

    #[test]
    fn zero_spans_auto_approve_allowed_when_low_severity_and_tier() {
        assert!(zero_spans_safe_to_auto_approve(1, 2));
        assert!(zero_spans_safe_to_auto_approve(2, 2));
    }

    #[test]
    fn zero_spans_auto_approve_blocked_by_severity_financial_repro() {
        // Repro: "$85,000 household income" / "$12,000 credit card debt" --
        // severity=3 (financial), zero PF spans. Must NOT be auto-approved.
        assert!(!zero_spans_safe_to_auto_approve(3, 2));
    }

    #[test]
    fn zero_spans_auto_approve_blocked_by_medical_severity() {
        assert!(!zero_spans_safe_to_auto_approve(4, 2));
    }

    #[test]
    fn zero_spans_auto_approve_blocked_by_target_tier() {
        assert!(!zero_spans_safe_to_auto_approve(1, 3));
    }

    // -- assign_review_tier --------------------------------------------------

    #[test]
    fn tier3_target_forces_high() {
        let e = vec![entity(0.99, "private_email")];
        assert!(matches!(assign_review_tier(&e, 1, 3), ReviewTier::High));
    }

    #[test]
    fn medical_severity_forces_high() {
        let e = vec![entity(0.99, "private_email")];
        assert!(matches!(assign_review_tier(&e, 3, 2), ReviewTier::High));
    }

    #[test]
    fn tier3_target_forces_high_even_with_zero_entities() {
        // Guards against the vacuous-truth trap: entities.iter().all(...) on an
        // empty slice is vacuously true, which would wrongly resolve to Easy if
        // the severity/target_tier check didn't short-circuit first (items.id=36).
        let e: Vec<PfEntityDecoded> = vec![];
        assert!(matches!(assign_review_tier(&e, 1, 3), ReviewTier::High));
    }

    #[test]
    fn medical_severity_forces_high_even_with_zero_entities() {
        let e: Vec<PfEntityDecoded> = vec![];
        assert!(matches!(assign_review_tier(&e, 3, 2), ReviewTier::High));
    }

    #[test]
    fn low_confidence_forces_high() {
        let e = vec![entity(0.65, "private_email")];
        assert!(matches!(assign_review_tier(&e, 1, 2), ReviewTier::High));
    }

    #[test]
    fn all_high_confidence_easy_category_is_easy() {
        let e = vec![entity(0.95, "private_email"), entity(0.92, "private_phone")];
        assert!(matches!(assign_review_tier(&e, 1, 2), ReviewTier::Easy));
    }

    #[test]
    fn easy_category_but_one_medium_score_is_medium() {
        let e = vec![entity(0.95, "private_email"), entity(0.75, "private_phone")];
        assert!(matches!(assign_review_tier(&e, 1, 2), ReviewTier::Medium));
    }

    #[test]
    fn contextual_category_is_medium_even_at_high_confidence() {
        let e = vec![entity(0.99, "private_date")];
        assert!(matches!(assign_review_tier(&e, 1, 2), ReviewTier::Medium));
    }

    // -- taxonomy_label ------------------------------------------------------

    #[test]
    fn taxonomy_known_categories() {
        assert_eq!(taxonomy_label("private_person"), "Name or identity");
        assert_eq!(taxonomy_label("private_address"), "Address or location");
        assert_eq!(taxonomy_label("private_email"), "Email address");
        assert_eq!(taxonomy_label("private_phone"), "Phone number");
        assert_eq!(taxonomy_label("private_url"), "Personal web address");
        assert_eq!(taxonomy_label("private_date"), "Personal date");
        assert_eq!(taxonomy_label("account_number"), "Account number");
        assert_eq!(taxonomy_label("secret"), "Sensitive value");
    }

    #[test]
    fn taxonomy_unknown_category_fallback() {
        assert_eq!(taxonomy_label("private_ssn"), "Sensitive information");
    }

    // -- generalization_suggestion -------------------------------------------

    #[test]
    fn suggestion_known_categories() {
        assert_eq!(
            generalization_suggestion("private_person"),
            Some("[person]".to_owned())
        );
        assert_eq!(
            generalization_suggestion("private_email"),
            Some("[email address]".to_owned())
        );
        assert_eq!(
            generalization_suggestion("account_number"),
            Some("[account number]".to_owned())
        );
    }

    #[test]
    fn suggestion_unknown_category_is_none() {
        assert_eq!(generalization_suggestion("private_ssn"), None);
    }
}
