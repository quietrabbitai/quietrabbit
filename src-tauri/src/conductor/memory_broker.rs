// src-tauri/src/conductor/memory_broker.rs
// Memory Broker — sole interface between the Conductor and memory stores.
// The Conductor never calls domain_context_store or plan_state_store directly.
//
// Called at Phase 3 INITIALIZE via assemble_context().
// Returns a ContextSlice the Conductor uses for prompt assembly.
// Lifecycle merges PersonalTrack + ContextSlice — Broker never touches PersonalTrack.
//
// Three retrieval tiers (ADR-013 Section 5.3):
//   Tier A — always loaded: Domain Context standing summary (hard token ceiling).
//             Quick Ask: Tier A skipped — no Domain Context loaded.
//   Tier B — session-loaded: domain_context_blocks + plan_state_blocks within
//             remaining budget after Tier A. Unified budget pool — DC first,
//             then PS against whatever remains. Only when topic_id is non-null.
//   Tier C — on-demand: stub only in Phase B. ConductorError::NotImplemented.
//             Layer 7 wires mid-session retrieval.
//
// Budget ownership: the Broker owns all budget computation (ADR-013 Section 5.8).
//   Input: model_context_window (raw context window of selected model).
//   Optional overrides: tier_a_ceiling, reserve_margin (env var defaults).
//   Tier B budget: remaining after Tier A actual usage + reserve margin.
//   Unused Tier A budget rolls over to Tier B — no fixed partitioning.
//
// Retrieval Eligibility Check (ADR-013 Section 5.6):
//   Pre-filters blocks by visibility_scope vs execution_tier ceiling.
//   Distinct from Gate1 — structural access control only, not privacy policy.
//   Gate1 remains the single abstraction authority.
//
// Policy-agnostic boundary: the Broker tracks an advisory sensitivity ceiling
//   (highest_sensitivity on ContextSlice) but makes no policy decisions.
//   Gate1 is the sole abstraction authority. No Gate logic runs here.
//   Sensitivity tracking is for diagnostics and logging only.
//
// Isolation invariant (ADR-013 Section 8.9, D6-301):
//   Privacy isolation scoped to (focus_id, topic_id).
//   Storage resolution additionally requires persona_id.
//   Persona is not a privacy boundary — Focus is the privacy container (D6-291).
//   Non-empty string assertions applied before any retrieval — fail closed.
//
// Quick Ask invariant (ADR-013 Section 9.7):
//   Quick Ask runs load Profile only — no Domain Context, no Plan State.
//   is_quick_ask=True → Tier A and Tier B both skipped.
//
// highest_sensitivity on ContextSlice:
//   Advisory metadata only — not used for routing or Gate1 decisions.
//   Gate1 operates at block level. For diagnostics and logging only.
//
// MemoryBroker is stateless — safe to instantiate per FocusRun.
// Conductor never imports domain_context_store or plan_state_store directly.
//
// Domain field strings (source_type, block_type, retrieval_tier, visibility_scope):
//   Oracle-faithful string fields. Python oracle uses string literals throughout,
//   and these values cross IPC and database boundaries as strings. Future enum
//   candidates post-migration — not changed here to avoid store boundary churn.
//
// Port of conductor/memory_broker.py (Phase C — D6-298, D6-301).
// assemble_context() and helpers are async: store functions are async in Rust
// (sqlx calls). Python oracle is sync only because Python stores are sync.

use std::env;

use chrono::Utc;

use crate::conductor::failure::ConductorError;
use crate::persistence::domain_context_store;
use crate::persistence::plan_state_store;

// ---------------------------------------------------------------------------
// Budget constants
// ---------------------------------------------------------------------------

fn default_reserve_margin() -> f64 {
    env::var("QR_MEMORY_RESERVE_MARGIN")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0.15)
}

fn default_tier_a_ceiling() -> i32 {
    env::var("QR_TIER_A_TOKEN_CEILING")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(512)
}

// Sensitivity preset ordering for ceiling computation.
// Unknown presets default to rank 3 (locked) — fail closed on unrecognised values.
fn preset_rank(preset: &str) -> i32 {
    match preset {
        "standard"  => 0,
        "sensitive" => 1,
        "private"   => 2,
        "locked"    => 3,
        _           => 3, // fail closed
    }
}

fn rank_to_preset(rank: i32) -> &'static str {
    match rank {
        0 => "standard",
        1 => "sensitive",
        2 => "private",
        _ => "locked",
    }
}

// ---------------------------------------------------------------------------
// MemoryBlock
// ---------------------------------------------------------------------------

/// A single retrieved memory block returned to the Conductor.
///
/// source_type:     "domain_context" | "plan_state"
/// block_type:      block_type from plan_state_blocks, or "knowledge" for
///                  domain context blocks.
/// retrieval_tier:  "tier_a" | "tier_b"
/// source_topic_id: provenance — which topic this block originated from.
///                  None for standing summary (Tier A) or if not applicable.
/// dependency_refs: provenance — block IDs this block was derived from.
///                  Carries sensitivity inheritance lineage for audit.
///
/// The Conductor receives MemoryBlocks — never opens stores directly.
/// Provenance fields (source_topic_id, dependency_refs) are available to
/// the system for audit and retrospective — not injected into prompts.
#[derive(Debug, Clone)]
pub struct MemoryBlock {
    pub block_id: String,
    pub content: String,
    pub source_type: String,
    pub block_type: String,
    pub visibility_scope: String,
    pub sensitivity_preset: String,
    pub token_estimate: i32,
    pub retrieval_tier: String,
    pub source_topic_id: Option<String>,
    pub dependency_refs: Vec<String>,
}

// ---------------------------------------------------------------------------
// ContextSlice
// ---------------------------------------------------------------------------

/// Assembled memory context for one focus run session.
/// Returned by MemoryBroker::assemble_context() to lifecycle at Phase 3 INITIALIZE.
/// Lifecycle merges this with PersonalTrack — never the Broker's job.
///
/// retrieved_blocks:       unified list across all sources and tiers.
/// token_budget_used:      actual tokens consumed by retrieved blocks.
/// token_budget_remaining: tokens available for prompt + user input + output.
/// highest_sensitivity:    ADVISORY ONLY — highest sensitivity_preset seen
///                         across retrieved blocks. Not used for routing or
///                         Gate1 decisions. Gate1 operates at block level.
///                         For diagnostics and logging only.
/// loaded_block_count:     blocks successfully loaded within budget.
/// deferred_block_count:   eligible blocks that exceeded budget (not loaded).
/// retrieval_timestamp:    when assemble_context() ran — for manifest delta.
/// active_topic_id:        the topic loaded, or None for unnamed/Quick Ask runs.
#[derive(Debug, Default)]
pub struct ContextSlice {
    pub retrieved_blocks: Vec<MemoryBlock>,
    pub token_budget_used: i32,
    pub token_budget_remaining: i32,
    pub highest_sensitivity: String,
    pub loaded_block_count: usize,
    pub deferred_block_count: usize,
    pub retrieval_timestamp: String,
    pub active_topic_id: Option<String>,
}

impl ContextSlice {
    pub fn new(retrieval_timestamp: String, active_topic_id: Option<String>) -> Self {
        Self {
            retrieved_blocks: Vec::new(),
            token_budget_used: 0,
            token_budget_remaining: 0,
            highest_sensitivity: "standard".to_string(),
            loaded_block_count: 0,
            deferred_block_count: 0,
            retrieval_timestamp,
            active_topic_id,
        }
    }

    /// Serialise retrieved block contents into a prompt-injectable string.
    /// Called by lifecycle to populate persona_context in prompt assembly.
    /// Blocks ordered: Tier A first (standing summary), then Tier B DC, then Tier B PS.
    /// Empty string returned if no blocks retrieved — graceful degradation.
    /// Conductor calls this — never joins block contents manually.
    pub fn render(&self) -> String {
        if self.retrieved_blocks.is_empty() {
            return String::new();
        }

        let mut parts: Vec<String> = Vec::new();

        // Tier A — standing summary
        let tier_a: Vec<&str> = self
            .retrieved_blocks
            .iter()
            .filter(|b| b.retrieval_tier == "tier_a" && !b.content.is_empty())
            .map(|b| b.content.as_str())
            .collect();
        if !tier_a.is_empty() {
            parts.push(tier_a.join("\n\n"));
        }

        // Tier B — domain context blocks
        let dc: Vec<&str> = self
            .retrieved_blocks
            .iter()
            .filter(|b| {
                b.retrieval_tier == "tier_b"
                    && b.source_type == "domain_context"
                    && !b.content.is_empty()
            })
            .map(|b| b.content.as_str())
            .collect();
        if !dc.is_empty() {
            parts.push(dc.join("\n\n"));
        }

        // Tier B — plan state blocks
        let ps: Vec<&str> = self
            .retrieved_blocks
            .iter()
            .filter(|b| {
                b.retrieval_tier == "tier_b"
                    && b.source_type == "plan_state"
                    && !b.content.is_empty()
            })
            .map(|b| b.content.as_str())
            .collect();
        if !ps.is_empty() {
            parts.push(ps.join("\n\n"));
        }

        parts.join("\n\n---\n\n").trim().to_string()
    }

    /// Explicitly drop block content from memory.
    /// Called by lifecycle immediately after render() to avoid retaining
    /// a second in-memory copy of sensitive context for the session duration.
    /// Replaces retrieved_blocks with an empty Vec, dropping all String heap
    /// allocations. Does NOT byte-zero the freed memory — zeroize is a
    /// post-migration hardening item, not in scope for Phase B.
    /// After clear(), render() returns empty string.
    pub fn clear(&mut self) {
        self.retrieved_blocks = Vec::new();
    }

    /// Raise the sensitivity ceiling if the new preset is higher.
    /// Unknown presets default to rank 3 (locked) — fail closed.
    /// highest_sensitivity is ADVISORY ONLY — not used for routing or Gate1.
    /// Broker is policy-agnostic; Gate1 is the sole abstraction authority.
    fn update_highest_sensitivity(&mut self, preset: Option<&str>) {
        let Some(preset) = preset else { return };
        let current_rank = preset_rank(&self.highest_sensitivity);
        let new_rank = preset_rank(preset);
        if new_rank > current_rank {
            self.highest_sensitivity = rank_to_preset(new_rank).to_string();
        }
    }
}

// ---------------------------------------------------------------------------
// MemoryBroker
// ---------------------------------------------------------------------------

/// Sole interface between the Conductor and memory stores.
/// Stateless — safe to instantiate per FocusRun in lifecycle Phase 3.
/// Conductor never imports domain_context_store or plan_state_store directly.
///
/// Privacy isolation scoped to (focus_id, topic_id) per D6-301.
/// Storage resolution additionally requires persona_id.
/// Persona is not an isolation boundary — Focus is the privacy container.
/// No cross-topic or cross-focus access.
/// Non-empty string assertions applied before retrieval — fail closed.
///
/// Policy-agnostic: broker does not apply Gate logic or abstraction policy.
/// Gate1 is the sole abstraction authority; broker delegates eligibility
/// checks to store layer (visibility_scope vs execution_tier filtering).
pub struct MemoryBroker;

impl MemoryBroker {
    pub fn new() -> Self {
        Self
    }

    /// Assemble the memory context slice for a focus run session.
    /// Called at Phase 3 INITIALIZE.
    ///
    /// Budget computation (Broker-owned, ADR-013 Section 5.8):
    ///   reserve = model_context_window * reserve_margin
    ///   available = model_context_window - reserve
    ///   tier_a_used = min(actual standing summary tokens, tier_a_ceiling)
    ///   tier_b_budget = available - tier_a_used
    ///   Unused Tier A budget rolls over to Tier B automatically.
    ///
    /// tier_a_ceiling: overrides QR_TIER_A_TOKEN_CEILING env var if provided.
    /// reserve_margin: overrides QR_MEMORY_RESERVE_MARGIN env var if provided.
    ///
    /// Quick Ask invariant: is_quick_ask=true → returns empty ContextSlice.
    ///   No Domain Context, no Plan State loaded.
    ///
    /// topic_id=None and not is_quick_ask → unnamed run.
    ///   Tier A (standing summary) loaded if domain_context.db exists.
    ///   Tier B skipped — no active topic.
    #[allow(clippy::too_many_arguments)] // Explicit architecture boundary; see D6-342/D6-346.
    pub async fn assemble_context(
        &self,
        user_id: &str,
        persona_id: &str,
        focus_id: &str,
        topic_id: Option<&str>,
        key_hex: &str,
        execution_tier: i32,
        model_context_window: i32,
        is_quick_ask: bool,
        tier_a_ceiling: Option<i32>,
        reserve_margin: Option<f64>,
    ) -> ContextSlice {
        // Isolation assertions — fail closed before any retrieval.
        assert!(!user_id.is_empty(),    "MemoryBroker: user_id must be non-empty");
        assert!(!persona_id.is_empty(), "MemoryBroker: persona_id must be non-empty");
        // persona_id required for storage path resolution — not a privacy boundary.
        // Privacy isolation is scoped to (focus_id, topic_id) per D6-301.
        assert!(!focus_id.is_empty(),   "MemoryBroker: focus_id must be non-empty");

        let timestamp = Utc::now().to_rfc3339();
        let mut slice = ContextSlice::new(timestamp, topic_id.map(|s| s.to_string()));

        let effective_tier_a_ceiling = tier_a_ceiling.unwrap_or_else(default_tier_a_ceiling);
        let effective_reserve_margin = reserve_margin.unwrap_or_else(default_reserve_margin);

        // Quick Ask invariant — no memory context loaded.
        if is_quick_ask {
            log::debug!(
                "memory_broker: Quick Ask run — skipping all context retrieval focus={}",
                focus_id
            );
            let reserve = (model_context_window as f64 * effective_reserve_margin) as i32;
            slice.token_budget_remaining = model_context_window - reserve;
            return slice;
        }

        // Budget computation.
        let reserve = (model_context_window as f64 * effective_reserve_margin) as i32;
        let available = model_context_window - reserve;

        // -- Tier A: Domain Context standing summary -------------------------
        let tier_a_used = self
            .load_tier_a(
                &mut slice,
                user_id,
                persona_id,
                focus_id,
                key_hex,
                effective_tier_a_ceiling,
            )
            .await;

        // Unused Tier A budget rolls over to Tier B — no fixed partitioning.
        let tier_b_budget = available - tier_a_used;
        slice.token_budget_used = tier_a_used;

        // -- Tier B: session-loaded blocks (only when topic is active) -------
        if let Some(tid) = topic_id {
            if tier_b_budget > 0 {
                self.load_tier_b(
                    &mut slice,
                    user_id,
                    persona_id,
                    focus_id,
                    tid,
                    key_hex,
                    execution_tier,
                    tier_b_budget,
                )
                .await;
            }
        }

        slice.token_budget_remaining =
            model_context_window - reserve - slice.token_budget_used;
        slice.loaded_block_count = slice.retrieved_blocks.len();

        log::debug!(
            "memory_broker: assembled context — focus={} topic={:?} \
             blocks={} deferred={} budget_used={} budget_remaining={} \
             highest_sensitivity={} tier={}",
            focus_id,
            topic_id,
            slice.loaded_block_count,
            slice.deferred_block_count,
            slice.token_budget_used,
            slice.token_budget_remaining,
            slice.highest_sensitivity,
            execution_tier,
        );

        slice
    }

    // -- Tier A --------------------------------------------------------------

    /// Load the Domain Context standing summary as a Tier A block.
    /// Returns actual tokens consumed — may be less than ceiling if summary is small.
    /// Unused Tier A budget rolls over to Tier B automatically.
    /// Returns 0 if domain_context.db does not exist or summary is empty.
    /// Store errors are logged as warnings and treated as absent (graceful degradation,
    /// matching Python oracle's try/except ImportError → return 0 behaviour).
    async fn load_tier_a(
        &self,
        slice: &mut ContextSlice,
        user_id: &str,
        persona_id: &str,
        focus_id: &str,
        key_hex: &str,
        tier_a_ceiling: i32,
    ) -> i32 {
        let summary = match domain_context_store::get_standing_summary(
            user_id, persona_id, focus_id, key_hex,
        )
        .await
        {
            Ok(Some(s)) => s,
            Ok(None) => {
                log::debug!(
                    "memory_broker: no standing summary for focus={} \
                     (db missing or empty) — full Tier A budget rolls to Tier B",
                    focus_id
                );
                return 0;
            }
            Err(e) => {
                log::warn!(
                    "memory_broker: get_standing_summary failed focus={} err={e}",
                    focus_id
                );
                return 0;
            }
        };

        if summary.content.is_empty() {
            log::debug!(
                "memory_broker: no standing summary for focus={} \
                 (db missing or empty) — full Tier A budget rolls to Tier B",
                focus_id
            );
            return 0;
        }

        // Charge actual token_count up to ceiling — not a fixed ceiling charge.
        let tokens_charged = summary.token_count.min(tier_a_ceiling);

        if summary.token_count > tier_a_ceiling {
            log::debug!(
                "memory_broker: standing summary truncated by Tier A ceiling \
                 focus={} actual={} ceiling={} charged={}",
                focus_id, summary.token_count, tier_a_ceiling, tokens_charged
            );
        }

        let block = MemoryBlock {
            block_id: "standing_summary".to_string(),
            content: summary.content,
            source_type: "domain_context".to_string(),
            block_type: "knowledge".to_string(),
            visibility_scope: "tier2_permitted".to_string(),
            sensitivity_preset: "standard".to_string(),
            token_estimate: tokens_charged,
            retrieval_tier: "tier_a".to_string(),
            source_topic_id: None,
            dependency_refs: Vec::new(),
        };
        slice.update_highest_sensitivity(Some("standard"));
        slice.retrieved_blocks.push(block);

        tokens_charged
    }

    // -- Tier B --------------------------------------------------------------

    /// Load Tier B blocks from both domain context and plan state.
    /// Uses a unified remaining budget — DC blocks loaded first, then PS
    /// blocks consume whatever remains. No hard 50/50 partition.
    /// Retrieval Eligibility Check applied by each store's get_eligible_blocks().
    /// Store errors are logged as warnings and treated as empty (graceful degradation).
    #[allow(clippy::too_many_arguments)] // Explicit architecture boundary; see D6-342/D6-346.
    async fn load_tier_b(
        &self,
        slice: &mut ContextSlice,
        user_id: &str,
        persona_id: &str,
        focus_id: &str,
        topic_id: &str,
        key_hex: &str,
        execution_tier: i32,
        budget: i32,
    ) {
        let dc_blocks = match domain_context_store::get_eligible_blocks(
            user_id, persona_id, focus_id, key_hex, execution_tier, None,
        )
        .await
        {
            Ok(blocks) => blocks,
            Err(e) => {
                log::warn!(
                    "memory_broker: get_eligible_blocks (DC) failed focus={} err={e}",
                    focus_id
                );
                Vec::new()
            }
        };

        let ps_blocks = match plan_state_store::get_eligible_blocks(
            user_id, persona_id, focus_id, topic_id, key_hex, execution_tier, None, None,
        )
        .await
        {
            Ok(blocks) => blocks,
            Err(e) => {
                log::warn!(
                    "memory_broker: get_eligible_blocks (PS) failed focus={} topic={} err={e}",
                    focus_id, topic_id
                );
                Vec::new()
            }
        };

        let mut tokens_loaded: i32 = 0;
        let mut deferred: usize = 0;

        // Domain context blocks first against unified budget.
        for dc_block in &dc_blocks {
            if tokens_loaded + dc_block.token_estimate > budget {
                deferred += 1;
                log::debug!(
                    "memory_broker: DC block deferred — excluded_reason=budget \
                     block={} token_estimate={} remaining={}",
                    dc_block.id, dc_block.token_estimate, budget - tokens_loaded,
                );
                continue;
            }
            let block = MemoryBlock {
                block_id: dc_block.id.clone(),
                content: dc_block.content.clone(),
                source_type: "domain_context".to_string(),
                block_type: "knowledge".to_string(),
                visibility_scope: dc_block.visibility_scope.clone(),
                sensitivity_preset: dc_block.sensitivity_preset.clone(),
                token_estimate: dc_block.token_estimate,
                retrieval_tier: "tier_b".to_string(),
                source_topic_id: Some(dc_block.source_topic_id.clone()),
                dependency_refs: dc_block.dependency_refs.clone(),
            };
            slice.update_highest_sensitivity(Some(&dc_block.sensitivity_preset));
            slice.retrieved_blocks.push(block);
            tokens_loaded += dc_block.token_estimate;
        }

        // Plan state blocks against whatever budget remains after DC.
        for ps_block in &ps_blocks {
            if tokens_loaded + ps_block.token_estimate > budget {
                deferred += 1;
                log::debug!(
                    "memory_broker: PS block deferred — excluded_reason=budget \
                     block={} token_estimate={} remaining={}",
                    ps_block.id, ps_block.token_estimate, budget - tokens_loaded,
                );
                continue;
            }
            let sensitivity = ps_block.sensitivity_preset.as_deref().unwrap_or("standard");
            let block = MemoryBlock {
                block_id: ps_block.id.clone(),
                content: ps_block.content.clone(),
                source_type: "plan_state".to_string(),
                block_type: ps_block.block_type.clone(),
                visibility_scope: ps_block.visibility_scope.clone(),
                sensitivity_preset: sensitivity.to_string(),
                token_estimate: ps_block.token_estimate,
                retrieval_tier: "tier_b".to_string(),
                source_topic_id: None,
                dependency_refs: ps_block.dependency_refs.clone(),
            };
            slice.update_highest_sensitivity(Some(sensitivity));
            slice.retrieved_blocks.push(block);
            tokens_loaded += ps_block.token_estimate;
        }

        slice.token_budget_used += tokens_loaded;
        slice.deferred_block_count += deferred;
    }

    // -- Tier C stub ---------------------------------------------------------

    /// Tier C — on-demand mid-session retrieval.
    /// Intentionally sync stub — no I/O in Phase B.
    /// Layer 7 wires full async implementation.
    /// Tier C blocks are task-scoped and discarded after task completion
    /// unless promoted to Plan State (ADR-013 Section 5.3).
    #[allow(dead_code)]
    #[allow(clippy::too_many_arguments)] // Explicit architecture boundary; see D6-342/D6-346.
    pub fn retrieve_additional_context(
        &self,
        _user_id: &str,
        _persona_id: &str,
        _focus_id: &str,
        _topic_id: &str,
        _key_hex: &str,
        _query: &str,
        _execution_tier: i32,
        _max_tokens: i32,
    ) -> Result<Vec<MemoryBlock>, ConductorError> {
        Err(ConductorError::NotImplemented {
            plain_language: "Tier C on-demand retrieval is not implemented in Phase B. \
                             Wire in Layer 7 when conductor-brief focus builds begin."
                .to_string(),
        })
    }
}

impl Default for MemoryBroker {
    fn default() -> Self {
        Self::new()
    }
}
