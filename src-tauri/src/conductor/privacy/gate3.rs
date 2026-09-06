// src-tauri/src/conductor/privacy/gate3.rs
//
// PG_GATE_3: cross-tier content promotion guardian with Privacy Filter integration.
//
// Gate ordering is load-bearing — do NOT reorder:
//   1. External-access ceiling block (items.id=439, Part 6e): blocked when
//      the candidate destination requires leaving the device
//      (ExternalAccess::from_legacy_tier(target_tier) != LocalOnly) AND the
//      Focus's own ceiling is LocalOnly. Deliberate behavior change at one
//      edge vs. the old target_tier > space_max_permitted_tier ordinal
//      check (e.g. ceiling=anonymous_required/target=unrestricted no longer
//      blocks here) — Part 6a's root-cause finding is that no live code
//      anywhere distinguishes old "Tier 2" from "Tier 3" as a real
//      capability difference; the only real gate is local-vs-not, and any
//      finer-grained provider selection happens downstream via
//      focus_provider_criteria/user_provider_preference, not this
//      structural comparison.
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

use crate::conductor::tokens::ExternalAccess;

use super::{
    coref,
    errors::DisclosureLogWriteError,
    fact_identity,
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

/// Minimum confidence score for Low tier. The span must meet this threshold
/// AND be in LOW_TIER_CATEGORIES. items.id=406: renamed from
/// EASY_SCORE_THRESHOLD (ReviewTier::Easy -> Low, decisions.id=754).
const LOW_SCORE_THRESHOLD: f32 = 0.90;

/// Minimum confidence for Medium tier. Any span below this forces High.
/// Errs toward High per D6-362.
const MEDIUM_SCORE_THRESHOLD: f32 = 0.70;

/// Categories that qualify for Low tier when a span exceeds LOW_SCORE_THRESHOLD.
/// Contextual categories (private_date, private_url, secret) default to Medium
/// even at high confidence. items.id=406: renamed from EASY_TIER_CATEGORIES.
const LOW_TIER_CATEGORIES: &[&str] = &["private_email", "private_phone", "account_number"];

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
    // items.id=439 (Part 6e): stays u8, NOT retyped to ExternalAccess --
    // genuinely dual-purpose today: Check 1's ceiling comparison below AND
    // gate3_with_pf's destination_risk_rating.unwrap_or(target_tier)
    // fallback (an unrelated risk-rating axis, item 406's territory).
    // Retyping would break that fallback's only source of a u8 when no live
    // per-provider rating is known. Converted internally, once, for Check 1
    // only via ExternalAccess::from_legacy_tier(target_tier) below.
    target_tier: u8,
    // items.id=439 (Part 6e): retyped from the former space_max_permitted_tier: u8.
    // Unlike target_tier below, this param has exactly one other use besides
    // Check 1 -- echoing into Gate3Result.space_max_permitted_tier (items.id=448,
    // untouched Option<u8> shape) -- so retyping it outright is safe.
    focus_external_access: ExternalAccess,
    execution_tier: u8,
    app_handle: Option<&tauri::AppHandle<tauri::Wry>>,
    // items.id=406 (decisions.id=753): live per-provider destination risk
    // (1=Low/2=Medium/3=High, from providers.risk_rating -- items.id=427
    // generalized tier3_providers into providers, column unchanged), MAX across
    // whatever destinations are currently active/selected. None when the
    // caller has no specific-provider context (e.g. executor.rs's generic
    // Focus-step promotion) -- falls back to target_tier itself, exactly
    // today's behavior. Replaces the old category-only "target_tier >= 3
    // always means High" assumption for callers that DO know their actual
    // destination set; the OR-condition structure itself is unchanged.
    destination_risk_rating: Option<u8>,
    // items.id=406 (decisions.id=756/757): identity needed to open
    // outputs.db/personal.db for the fact-identity persistence cascade
    // (prior-decision query, pf_fact_mentions, pf_standing_preferences).
    // Neither was available inside gate3 before this -- only inside the
    // DisclosureLogger passed in as `logger`, which stays scoped to just
    // disclosure_log writes. Both real call sites (request_tier3_gate3_review
    // and executor.rs's Step 13, via StepContext) always have real values;
    // an empty key_hex gracefully degrades the persistence cascade to
    // "always ask" (see partition_by_prior_decision), never a hard error.
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
) -> Result<Gate3Result, DisclosureLogWriteError> {
    // Check 1: external-access ceiling block — fires first, before any other
    // check. See this function's header comment for why this is a
    // deliberate behavior change at one edge vs. the old ordinal comparison.
    let candidate_requires_external =
        ExternalAccess::from_legacy_tier(target_tier) != ExternalAccess::LocalOnly;
    if candidate_requires_external && focus_external_access == ExternalAccess::LocalOnly {
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
                category: None,
                fact_key: None,
            })
            .await?;

        return Ok(Gate3Result {
            blocked: true,
            plain_language: Some(
                "This content can't be shared with a higher-tier service \
                 from this Focus. [Change Focus settings] [Use local only]"
                    .to_string(),
            ),
            target_tier: Some(target_tier),
            // items.id=448 owns this field's real redesign; as_legacy_tier()
            // is a lossy but display-only round-trip until then.
            space_max_permitted_tier: Some(focus_external_access.as_legacy_tier()),
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
                destination_risk_rating,
                user_id,
                persona_id,
                key_hex,
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
                category: None,
                fact_key: None,
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
            category: None,
            fact_key: None,
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
    destination_risk_rating: Option<u8>,
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
) -> Result<Gate3Result, DisclosureLogWriteError> {
    // items.id=406: the value that actually feeds the High-forcing side of
    // assign_review_tier_for_span's OR-condition -- a live per-provider risk
    // rating when the caller knows its destination set, else target_tier
    // itself (today's exact behavior, unchanged).
    let destination_risk = destination_risk_rating.unwrap_or(target_tier);
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
                    category: None,
                    fact_key: None,
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
                    category: None,
                    fact_key: None,
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
                        category: None,
                        fact_key: None,
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
                    category: None,
                    fact_key: None,
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

    // items.id=406 (decisions.id=756/757): partition entities into those
    // with an already-made prior decision (silently reapplied, still
    // disclosure-logged) and those that genuinely need interactive review.
    // Must happen BEFORE the zero-spans check and span/tier assembly below
    // -- only the needs-review subset should ever become a ConsentSpanItem
    // in the emitted payload, and an all-auto-resolved batch may mean
    // nothing left to review even when PF returned entities.
    let partition = partition_by_prior_decision(
        logger,
        user_id,
        persona_id,
        key_hex,
        step_id,
        focus_run_id,
        execution_tier,
        entities,
    )
    .await?;
    if let Some(result) = partition.early_result {
        return Ok(result);
    }
    let entities = partition.needs_review;

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
        && zero_spans_safe_to_auto_approve(content_sensitivity_severity, destination_risk)
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
                category: None,
                fact_key: None,
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
    let spans = build_consent_spans(&entities, content_sensitivity_severity, destination_risk);
    let no_spans_forced_high = spans.is_empty();

    // items.id=406: payload-level review_tier is now only meaningful for the
    // empty-spans-forced-High case above (no span exists to carry a tier).
    // When spans is non-empty, this is just the worst (most restrictive)
    // tier among them -- the frontend groups rows by each span's own
    // review_tier, not this field, for everything except that edge case.
    let overall_review_tier = spans
        .iter()
        .map(|s| tier_rank(&s.review_tier))
        .max()
        .map(rank_to_tier)
        .unwrap_or(ReviewTier::High); // reaching here with empty spans is always forced-High

    let payload = ConsentRequestPayload {
        focus_run_id: focus_run_id.to_owned(),
        focus_name: focus_name.to_owned(),
        review_tier: overall_review_tier,
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
            category: None,
            fact_key: None,
        })
        .await?;

    let _ = handle.emit("consent_request", &payload);

    Ok(Gate3Result {
        pending_consent: true,
        ..Gate3Result::default()
    })
}

// ---------------------------------------------------------------------------
// items.id=406 (decisions.id=756/757): fact-identity persistence cascade
// ---------------------------------------------------------------------------

/// Result of partitioning one PF pass's entities by prior-decision lookup.
struct PartitionOutcome {
    /// Entities that still need interactive review, paired with whatever
    /// fact_key (if any) gate3 resolved for them -- carried into
    /// `ConsentSpanItem.fact_key` so a future decision can be found again.
    needs_review: Vec<(PfEntityDecoded, Option<String>)>,
    /// `Some` only when EVERY entity in this pass was silently reapplied
    /// (never `Some` when `entities` was empty to begin with -- that case
    /// is left to gate3_with_pf's existing zero-spans handling unchanged).
    early_result: Option<Gate3Result>,
}

/// Resolves one entity's stable fact identity via the three-layer
/// deterministic cascade (decisions.id=757): Layer 1 content-hash -> Layer 2
/// within-conversation coreference -> Layer 3 entity_facts match. `None` if
/// none of the three resolve it -- the fact is treated as brand new
/// (unchanged fallback: always ask).
async fn resolve_fact_key(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
    entity: &PfEntityDecoded,
    prior_mentions: &[coref::PfFactMention],
) -> Option<String> {
    // Layer 1: stable content-hash identity (fact_identity.rs).
    if let Some(canonical) = fact_identity::canonicalize_fact(&entity.label, &entity.span_text) {
        return Some(fact_identity::fact_hash(&entity.label, &canonical));
    }

    // Layer 2: within-conversation coreference (coref.rs), scoped to this
    // focus_run_id via `prior_mentions` (already loaded by the caller).
    if let Some(fact_key) =
        coref::resolve_within_conversation(prior_mentions, &entity.label, &entity.span_text)
    {
        return Some(fact_key);
    }

    // Layer 3: entity_facts value match (fact_identity.rs). Gracefully
    // degrades to unresolved on any DB error -- this is an optimization
    // layer, not a privacy control; "ask" is always the safe fallback.
    match fact_identity::resolve_against_entity_facts(
        user_id,
        persona_id,
        key_hex,
        &entity.label,
        &entity.span_text,
    )
    .await
    {
        Ok(resolved) => resolved,
        Err(e) => {
            log::warn!(
                "gate3: Layer 3 entity_facts resolution failed, treating as unresolved: {e}"
            );
            None
        }
    }
}

/// True if `decision` is the blocking choice ("nothing leaves for this
/// fact"). Matches the string values write_element_consent_decisions_conn
/// and this module's own auto-reapply writer both use.
fn is_keep_private(decision: &str) -> bool {
    decision == "keep_private"
}

#[allow(clippy::too_many_arguments)]
async fn partition_by_prior_decision<L: DisclosureLogger>(
    logger: &L,
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
    step_id: &str,
    focus_run_id: &str,
    execution_tier: u8,
    entities: Vec<PfEntityDecoded>,
) -> Result<PartitionOutcome, DisclosureLogWriteError> {
    if entities.is_empty() {
        return Ok(PartitionOutcome {
            needs_review: vec![],
            early_result: None,
        });
    }

    let prior_mentions = crate::persistence::output_store::load_fact_mentions_for_run(
        user_id,
        persona_id,
        key_hex,
        focus_run_id,
    )
    .await
    .unwrap_or_else(|e| {
        log::warn!("gate3: failed to load prior fact mentions, Layer 2 has nothing to resolve against this call: {e}");
        vec![]
    });

    let mut needs_review = Vec::with_capacity(entities.len());
    let mut auto_resolved_decisions: Vec<String> = Vec::new();

    for entity in entities {
        let fact_key =
            resolve_fact_key(user_id, persona_id, key_hex, &entity, &prior_mentions).await;

        let prior_decision = match &fact_key {
            None => None,
            Some(fk) => {
                // Persona-scoped standing preference takes priority over the
                // conversation-scoped decision when both exist -- it's the
                // more deliberate, explicit choice (decisions.id=756).
                match crate::persistence::personal_store::find_standing_preference(
                    user_id, persona_id, key_hex, fk,
                )
                .await
                {
                    Ok(Some(pref)) => {
                        Some((pref.decision, pref.suggestion_text, pref.user_modified_text))
                    }
                    Ok(None) => {
                        match crate::persistence::output_store::find_consent_decision_for_fact(
                            user_id,
                            persona_id,
                            key_hex,
                            focus_run_id,
                            fk,
                        )
                        .await
                        {
                            Ok(Some(prior)) => Some((
                                prior.decision,
                                prior.suggestion_text,
                                prior.user_modified_text,
                            )),
                            Ok(None) => None,
                            Err(e) => {
                                log::warn!(
                                    "gate3: conversation-scoped prior-decision query failed, \
                                     treating as unresolved: {e}"
                                );
                                None
                            }
                        }
                    }
                    Err(e) => {
                        log::warn!(
                            "gate3: standing-preference query failed, treating as unresolved: {e}"
                        );
                        None
                    }
                }
            }
        };

        match prior_decision {
            Some((decision, suggestion_text, user_modified_text)) => {
                // Silent reapplication -- still logged, every time (D6-198
                // append-only invariant; NOT skipping the check).
                logger
                    .write(DisclosureLogEntry {
                        step_id: step_id.to_string(),
                        focus_run_id: focus_run_id.to_string(),
                        execution_tier,
                        abstraction_tier: None,
                        provider: None,
                        fields_shared: vec![],
                        fields_abstracted: IndexMap::new(),
                        fields_withheld: vec![],
                        override_declined: false,
                        event_type: "gate3_fact_reapplied".to_string(),
                        category: Some(entity.label.clone()),
                        fact_key: fact_key.clone(),
                    })
                    .await?;

                // Best-effort audit row -- disclosure_log above is the
                // fatal-path write; a failure here doesn't block the gate.
                if let Some(fk) = &fact_key {
                    if let Err(e) =
                        crate::persistence::output_store::write_auto_reapplied_consent_decision(
                            user_id,
                            persona_id,
                            key_hex,
                            focus_run_id,
                            &decision,
                            suggestion_text.as_deref(),
                            user_modified_text.as_deref(),
                            &entity.label,
                            fk,
                            &entity.span_text,
                        )
                        .await
                    {
                        log::warn!("gate3: failed to write auto-reapplied consent_decisions audit row: {e}");
                    }
                }

                auto_resolved_decisions.push(decision);
            }
            None => {
                // Genuinely new (or unresolved) fact -- record a mention
                // (best-effort) so a LATER span in this same conversation
                // can coref against it even before this one is decided,
                // then surface it for interactive review.
                if let Some(fk) = &fact_key {
                    if let Err(e) = crate::persistence::output_store::insert_fact_mention(
                        user_id,
                        persona_id,
                        key_hex,
                        focus_run_id,
                        &entity.label,
                        fk,
                        &entity.span_text,
                    )
                    .await
                    {
                        log::warn!("gate3: failed to record pf_fact_mentions row: {e}");
                    }
                }
                needs_review.push((entity, fact_key));
            }
        }
    }

    let early_result = if needs_review.is_empty() {
        // Every entity in this pass was silently reapplied. Mirrors the
        // interactive flow's own "run stops if ALL kept private, continues
        // otherwise" rule (PRIVACY_GUARDIAN_GATE_SPEC.md post-decision
        // state) -- applied here since there is no interactive summary to
        // show for an all-auto-resolved pass.
        let all_kept_private = auto_resolved_decisions.iter().all(|d| is_keep_private(d));
        Some(if all_kept_private {
            Gate3Result {
                blocked: true,
                plain_language: Some(
                    "This content was kept private based on your earlier decision for this \
                     information. [Use local only]"
                        .to_string(),
                ),
                ..Gate3Result::default()
            }
        } else {
            Gate3Result {
                approved: true,
                ..Gate3Result::default()
            }
        })
    } else {
        None
    };

    Ok(PartitionOutcome {
        needs_review,
        early_result,
    })
}

// ---------------------------------------------------------------------------
// Span assembly helpers
// ---------------------------------------------------------------------------

fn build_consent_spans(
    entities: &[(PfEntityDecoded, Option<String>)],
    content_sensitivity_severity: u8,
    destination_risk: u8,
) -> Vec<ConsentSpanItem> {
    entities
        .iter()
        .map(|(e, fact_key)| ConsentSpanItem {
            span_id: Uuid::new_v4().to_string(),
            category: e.label.clone(),
            user_label: taxonomy_label(&e.label),
            original_text: e.span_text.clone(),
            suggestion: generalization_suggestion(&e.label),
            start_byte: e.start_byte,
            end_byte: e.end_byte,
            score: e.score,
            review_tier: assign_review_tier_for_span(
                e,
                content_sensitivity_severity,
                destination_risk,
            ),
            // items.id=406: resolved by partition_by_prior_decision before
            // this span was ever created (a resolved-and-matched fact would
            // have been auto-reapplied there instead, never reaching here)
            // -- echoed to the frontend so a future decision can be found.
            fact_key: fact_key.clone(),
        })
        .collect()
}

/// True only when PF returning zero spans is safe to auto-approve without a
/// consent gate: severity and destination_risk must both be below the
/// High-forcing thresholds used by `assign_review_tier_for_span`. Kept in
/// sync with that function's `destination_risk >= 3 ||
/// content_sensitivity_severity >= 3` condition — this is the
/// entities-independent half of the same rule (items.id=36).
fn zero_spans_safe_to_auto_approve(content_sensitivity_severity: u8, destination_risk: u8) -> bool {
    !(content_sensitivity_severity >= 3 || destination_risk >= 3)
}

/// items.id=406 (decisions.id=754): assigns ONE span's own review tier,
/// independent of every other span in the same batch. Previously
/// (`assign_review_tier`) this was computed once for the whole entity slice
/// -- Low tier required EVERY span to individually clear the threshold, so
/// one Medium-confidence span silently downgraded every other span's
/// section too. The three-section review screen needs each fact
/// independently assigned (fact profile x destination profile), so each
/// span is now evaluated entirely on its own, sharing only the two
/// call-level inputs (content severity, destination risk) that are
/// genuinely shared across the whole piece of content.
fn assign_review_tier_for_span(
    entity: &PfEntityDecoded,
    content_sensitivity_severity: u8,
    destination_risk: u8,
) -> ReviewTier {
    // High: Medical/Financial context, high-risk destination, or low-confidence span.
    if destination_risk >= 3 || content_sensitivity_severity >= 3 {
        return ReviewTier::High;
    }
    if entity.score < MEDIUM_SCORE_THRESHOLD {
        return ReviewTier::High;
    }
    // Low: high-confidence AND a structural PII category.
    if entity.score >= LOW_SCORE_THRESHOLD && LOW_TIER_CATEGORIES.contains(&entity.label.as_str()) {
        return ReviewTier::Low;
    }
    ReviewTier::Medium
}

/// Ordering helper for `ConsentRequestPayload.review_tier`'s "worst tier
/// across all spans" fallback (items.id=406) -- Low < Medium < High.
fn tier_rank(tier: &ReviewTier) -> u8 {
    match tier {
        ReviewTier::Low => 0,
        ReviewTier::Medium => 1,
        ReviewTier::High => 2,
    }
}

fn rank_to_tier(rank: u8) -> ReviewTier {
    match rank {
        0 => ReviewTier::Low,
        1 => ReviewTier::Medium,
        _ => ReviewTier::High,
    }
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

    // -- assign_review_tier_for_span ------------------------------------------
    // items.id=406: refactored from a whole-batch assign_review_tier(entities)
    // to a per-entity assign_review_tier_for_span(entity) -- each span is now
    // evaluated entirely on its own, sharing only the two call-level inputs
    // (content severity, destination risk). "target_tier" in the old tests
    // below is now "destination_risk" -- same >= 3 forced-High semantics,
    // just fed a live per-provider rating instead of a raw category constant.

    #[test]
    fn high_destination_risk_forces_high() {
        let e = entity(0.99, "private_email");
        assert!(matches!(
            assign_review_tier_for_span(&e, 1, 3),
            ReviewTier::High
        ));
    }

    #[test]
    fn medical_severity_forces_high() {
        let e = entity(0.99, "private_email");
        assert!(matches!(
            assign_review_tier_for_span(&e, 3, 2),
            ReviewTier::High
        ));
    }

    #[test]
    fn low_confidence_forces_high() {
        let e = entity(0.65, "private_email");
        assert!(matches!(
            assign_review_tier_for_span(&e, 1, 2),
            ReviewTier::High
        ));
    }

    #[test]
    fn high_confidence_low_tier_category_is_low() {
        let e = entity(0.95, "private_email");
        assert!(matches!(
            assign_review_tier_for_span(&e, 1, 2),
            ReviewTier::Low
        ));
    }

    #[test]
    fn each_span_independent_one_medium_score_does_not_downgrade_a_sibling() {
        // items.id=406's core behavior change: under the old whole-batch
        // assign_review_tier, one Medium-confidence sibling span silently
        // pulled a High-confidence Low-tier-category span down to Medium too
        // (entities.iter().all(...) required EVERY span to individually
        // clear the Low threshold). Each span must now be judged only on its
        // own score/category -- a High-confidence email is Low regardless of
        // what else is in the same PF pass.
        let high_conf_email = entity(0.95, "private_email");
        let medium_conf_phone = entity(0.75, "private_phone");
        assert!(matches!(
            assign_review_tier_for_span(&high_conf_email, 1, 2),
            ReviewTier::Low
        ));
        assert!(matches!(
            assign_review_tier_for_span(&medium_conf_phone, 1, 2),
            ReviewTier::Medium
        ));
    }

    #[test]
    fn contextual_category_is_medium_even_at_high_confidence() {
        let e = entity(0.99, "private_date");
        assert!(matches!(
            assign_review_tier_for_span(&e, 1, 2),
            ReviewTier::Medium
        ));
    }

    #[test]
    fn low_destination_risk_does_not_force_high() {
        // items.id=406 (decisions.id=753): a Low-rated destination (e.g. an
        // anonymous Tier 2 provider) must not force High the way the old
        // hardcoded target_tier>=3 assumption did for every Tier 3 provider
        // regardless of its actual posture.
        let e = entity(0.95, "private_email");
        assert!(matches!(
            assign_review_tier_for_span(&e, 1, 1),
            ReviewTier::Low
        ));
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
