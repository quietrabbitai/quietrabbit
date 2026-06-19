// src-tauri/src/conductor/failure.rs
//
// Failure taxonomy and handler for the Conductor execution engine.
// Ported from conductor/failure.py.
//
// ConductorError: typed enum covering all F1-F10 + F_SYSTEM error variants.
//   FailureHandler::handle() matches on &ConductorError — no dyn Error
//   downcasting. DEC-002's error.is::<T>() pattern does not work on
//   dyn Error and is entirely replaced here.
//
// FailureResult: the structured outcome returned to StepExecutor/FocusRun.
//   action and severity are typed enums, serialized to snake_case strings
//   for IPC/JSON — frontend sees identical values to Python oracle.
//
// FailureHandler: stateless. Caller (StepExecutor) tracks retry_count.
//   handle() is synchronous — no async work occurs here.
//
// Updated as part of Phase A codebase rename (D6-224, D6-225):
//   path_id -> focus_id on FailureResult and handle() signature.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use thiserror::Error;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Maximum retries before escalating to offer_tier2 or await_user.
/// Python oracle: MAX_RETRIES = 3
pub const MAX_RETRIES: u32 = 3;

// ---------------------------------------------------------------------------
// ConductorError — full error taxonomy
// ---------------------------------------------------------------------------

/// All error variants the Conductor can encounter during a focus run.
/// FailureHandler::handle() matches on this enum — exhaustive, no downcasting.
///
/// Each variant carries a plain_language String — the human-readable message
/// for the UI. Constructed by the raising site (providers, gates, stores).
///
/// Python oracle: providers/errors.py exception hierarchy.
#[derive(Debug, Error)]
pub enum ConductorError {
    // F1 — Local model errors
    #[error("{plain_language}")]
    OllamaUnavailable { plain_language: String },
    #[error("{plain_language}")]
    OllamaTimeout { plain_language: String },
    #[error("{plain_language}")]
    OllamaGeneration { plain_language: String },
    #[error("{plain_language}")]
    OllamaInvalidRequest { plain_language: String },

    // F2 — Quality
    #[error("{plain_language}")]
    QualityBelowFloor { plain_language: String },

    // F3 — Context window
    #[error("{plain_language}")]
    ContextWindowExceeded { plain_language: String },

    // F4 — Privacy gate hard block
    #[error("{plain_language}")]
    PrivacyGateBlocked { plain_language: String },
    #[error("{plain_language}")]
    ContentPromotionBlocked { plain_language: String },

    // F5 — Security checker
    #[error("{plain_language}")]
    SecurityCheckerFlag { plain_language: String },

    // F6 — Inbound contamination
    #[error("{plain_language}")]
    InboundContamination { plain_language: String },

    // F7 — personal.db unavailable
    #[error("{plain_language}")]
    PersonalDbNotFound { plain_language: String },
    #[error("{plain_language}")]
    PersonalDbDecryption { plain_language: String },

    // F8 — Snapshot write failure
    #[error("{plain_language}")]
    SnapshotWrite { plain_language: String },

    // F9 — Loop detection
    #[error("{plain_language}")]
    LoopDetected { plain_language: String },

    // F10 — Tier 2/3 provider errors
    #[error("{plain_language}")]
    MissingApiKey { plain_language: String },
    #[error("{plain_language}")]
    InvalidApiKey { plain_language: String },
    #[error("{plain_language}")]
    ProviderRateLimit { plain_language: String },
    #[error("{plain_language}")]
    ProviderTimeout { plain_language: String },
    #[error("{plain_language}")]
    ProviderUnavailable { plain_language: String },
    #[error("{plain_language}")]
    Provider { plain_language: String },          // generic provider error

    // F_SYSTEM — fatal: integrity, audit, misconfiguration
    #[error("{plain_language}")]
    TaxonomyIntegrity { plain_language: String },
    #[error("{plain_language}")]
    DatabaseMigration { plain_language: String },
    #[error("{plain_language}")]
    DisclosureLogWrite { plain_language: String },
    #[error("{plain_language}")]
    UnknownProvider { plain_language: String },
}

impl ConductorError {
    /// The human-readable message for this error.
    /// Always present — every variant carries plain_language.
    pub fn plain_language(&self) -> &str {
        match self {
            Self::OllamaUnavailable { plain_language }
            | Self::OllamaTimeout { plain_language }
            | Self::OllamaGeneration { plain_language }
            | Self::OllamaInvalidRequest { plain_language }
            | Self::QualityBelowFloor { plain_language }
            | Self::ContextWindowExceeded { plain_language }
            | Self::PrivacyGateBlocked { plain_language }
            | Self::ContentPromotionBlocked { plain_language }
            | Self::SecurityCheckerFlag { plain_language }
            | Self::InboundContamination { plain_language }
            | Self::PersonalDbNotFound { plain_language }
            | Self::PersonalDbDecryption { plain_language }
            | Self::SnapshotWrite { plain_language }
            | Self::LoopDetected { plain_language }
            | Self::MissingApiKey { plain_language }
            | Self::InvalidApiKey { plain_language }
            | Self::ProviderRateLimit { plain_language }
            | Self::ProviderTimeout { plain_language }
            | Self::ProviderUnavailable { plain_language }
            | Self::Provider { plain_language }
            | Self::TaxonomyIntegrity { plain_language }
            | Self::DatabaseMigration { plain_language }
            | Self::DisclosureLogWrite { plain_language }
            | Self::UnknownProvider { plain_language } => plain_language.as_str(),
        }
    }
}

// ---------------------------------------------------------------------------
// FailureAction
// ---------------------------------------------------------------------------

/// What the Conductor should do after a failure.
/// Python oracle: action Literal on FailureResult.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureAction {
    Retry,
    OfferTier2,
    OfferCompact,
    AwaitUser,
    Stop,
    Degrade,
    HoldForGate,
    AwaitFloorConsent,
}

// ---------------------------------------------------------------------------
// FailureSeverity
// ---------------------------------------------------------------------------

/// How urgently the failure should be surfaced to the user.
/// Python oracle: severity Literal on FailureResult.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureSeverity {
    Info,
    Suggest,
    Require,
    Stop,
    Pause,
}

// ---------------------------------------------------------------------------
// FailureResult
// ---------------------------------------------------------------------------

/// The structured outcome of a failure handler decision.
/// Returned to StepExecutor / FocusRun lifecycle.
/// Python oracle: FailureResult dataclass in conductor/failure.py.
///
/// metadata: structured payload for floor consent, Gate3 review, etc.
///   HashMap (not IndexMap) — metadata is unordered key/value pairs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FailureResult {
    pub action: FailureAction,
    pub failure_mode: Option<String>,             // F1-F10, F_SYSTEM, F_UNEXPECTED, or None
    pub plain_language: String,
    pub is_recoverable: bool,
    pub severity: FailureSeverity,
    pub step_id: Option<String>,
    pub focus_id: Option<String>,
    pub metadata: Option<HashMap<String, serde_json::Value>>,
}

// ---------------------------------------------------------------------------
// FailureHandler
// ---------------------------------------------------------------------------

/// Maps ConductorError variants to FailureResult decisions.
/// Stateless — caller (StepExecutor) tracks retry_count per step.
/// Python oracle: FailureHandler class in conductor/failure.py.
pub struct FailureHandler {
    /// The maximum tier permitted for this focus run's persona.
    /// Controls whether offer_tier2 or local-only fallback is used.
    /// Valid range: 1-3. Enforced by debug_assert in new().
    /// Python oracle: space_max_permitted_tier on FailureHandler.__init__
    pub space_max_permitted_tier: u8,
}

impl FailureHandler {
    pub fn new(space_max_permitted_tier: u8) -> Self {
        debug_assert!(
            (1..=3).contains(&space_max_permitted_tier),
            "space_max_permitted_tier must be 1, 2, or 3 — got {}",
            space_max_permitted_tier
        );
        Self { space_max_permitted_tier }
    }

    /// Map a ConductorError to a FailureResult.
    /// Python oracle: FailureHandler.handle()
    pub fn handle(
        &self,
        error: &ConductorError,
        step_id: Option<&str>,
        focus_id: Option<&str>,
        retry_count: u32,
    ) -> FailureResult {
        let msg = error.plain_language().to_owned();
        let sid = step_id.map(|s| s.to_owned());
        let fid = focus_id.map(|s| s.to_owned());

        match error {
            // F_SYSTEM — fatal: integrity failures, audit failures, misconfiguration
            ConductorError::TaxonomyIntegrity { .. }
            | ConductorError::DatabaseMigration { .. }
            | ConductorError::DisclosureLogWrite { .. }
            | ConductorError::UnknownProvider { .. } => FailureResult {
                action: FailureAction::Stop,
                failure_mode: Some("F_SYSTEM".to_owned()),
                plain_language: msg,
                is_recoverable: false,
                severity: FailureSeverity::Stop,
                step_id: sid,
                focus_id: fid,
                metadata: None,
            },

            // F1 — Ollama unavailable: tier branch determines action
            ConductorError::OllamaUnavailable { .. } => {
                self.handle_f1_unavailable(msg, sid, fid)
            }

            // F1 — Timeout / generation failure: retry up to MAX_RETRIES
            ConductorError::OllamaTimeout { .. }
            | ConductorError::OllamaGeneration { .. } => {
                if retry_count >= MAX_RETRIES {
                    return self.escalate_failed_retry("F1", sid, fid);
                }
                FailureResult {
                    action: FailureAction::Retry,
                    failure_mode: Some("F1".to_owned()),
                    plain_language: msg,
                    is_recoverable: true,
                    severity: FailureSeverity::Require,
                    step_id: sid,
                    focus_id: fid,
                    metadata: None,
                }
            }

            // F1 — Invalid request: non-recoverable
            ConductorError::OllamaInvalidRequest { .. } => FailureResult {
                action: FailureAction::Stop,
                failure_mode: Some("F1".to_owned()),
                plain_language: msg,
                is_recoverable: false,
                severity: FailureSeverity::Stop,
                step_id: sid,
                focus_id: fid,
                metadata: None,
            },

            // F2 — Quality below floor: tier + retry branch
            ConductorError::QualityBelowFloor { .. } => {
                self.handle_f2(msg, sid, fid, retry_count)
            }

            // F3 — Context window exceeded
            ConductorError::ContextWindowExceeded { .. } => FailureResult {
                action: FailureAction::OfferCompact,
                failure_mode: Some("F3".to_owned()),
                plain_language: msg,
                is_recoverable: true,
                severity: FailureSeverity::Require,
                step_id: sid,
                focus_id: fid,
                metadata: None,
            },

            // F4 — Privacy gate hard block
            ConductorError::PrivacyGateBlocked { .. }
            | ConductorError::ContentPromotionBlocked { .. } => FailureResult {
                action: FailureAction::AwaitUser,
                failure_mode: Some("F4".to_owned()),
                plain_language: msg,
                is_recoverable: true,
                severity: FailureSeverity::Stop,
                step_id: sid,
                focus_id: fid,
                metadata: None,
            },

            // F5 — Security checker flag: non-recoverable
            ConductorError::SecurityCheckerFlag { .. } => FailureResult {
                action: FailureAction::Stop,
                failure_mode: Some("F5".to_owned()),
                plain_language: msg,
                is_recoverable: false,
                severity: FailureSeverity::Stop,
                step_id: sid,
                focus_id: fid,
                metadata: None,
            },

            // F6 — Inbound contamination: hold for gate review
            ConductorError::InboundContamination { .. } => FailureResult {
                action: FailureAction::HoldForGate,
                failure_mode: Some("F6".to_owned()),
                plain_language: msg,
                is_recoverable: true,
                severity: FailureSeverity::Require,
                step_id: sid,
                focus_id: fid,
                metadata: None,
            },

            // F7 — personal.db unavailable: non-recoverable
            ConductorError::PersonalDbNotFound { .. }
            | ConductorError::PersonalDbDecryption { .. } => FailureResult {
                action: FailureAction::Stop,
                failure_mode: Some("F7".to_owned()),
                plain_language: msg,
                is_recoverable: false,
                severity: FailureSeverity::Stop,
                step_id: sid,
                focus_id: fid,
                metadata: None,
            },

            // F8 — Snapshot write failure: degrade (continue without checkpointing)
            // Plain language is invariant — provider message is discarded (matches Python oracle).
            ConductorError::SnapshotWrite { .. } => FailureResult {
                action: FailureAction::Degrade,
                failure_mode: Some("F8".to_owned()),
                plain_language: concat!(
                    "Quiet Rabbit couldn't save your progress checkpoint. ",
                    "Your work will continue but can't be resumed if interrupted. ",
                    "[Continue] [Stop and save manually]",
                ).to_owned(),
                is_recoverable: true,
                severity: FailureSeverity::Suggest,
                step_id: sid,
                focus_id: fid,
                metadata: None,
            },

            // F9 — Loop detected: non-recoverable stop
            // Plain language is invariant — provider message is discarded (matches Python oracle).
            ConductorError::LoopDetected { .. } => FailureResult {
                action: FailureAction::Stop,
                failure_mode: Some("F9".to_owned()),
                plain_language: concat!(
                    "Quiet Rabbit detected a loop and stopped to protect ",
                    "your session. Your work is saved. [Get help]",
                ).to_owned(),
                is_recoverable: false,
                severity: FailureSeverity::Stop,
                step_id: sid,
                focus_id: fid,
                metadata: None,
            },

            // F10 — API key problems: surface to user
            ConductorError::MissingApiKey { .. }
            | ConductorError::InvalidApiKey { .. } => FailureResult {
                action: FailureAction::AwaitUser,
                failure_mode: Some("F10".to_owned()),
                plain_language: msg,
                is_recoverable: true,
                severity: FailureSeverity::Require,
                step_id: sid,
                focus_id: fid,
                metadata: None,
            },

            // F10 — Rate limit: retry with suggest severity
            ConductorError::ProviderRateLimit { .. } => {
                if retry_count >= MAX_RETRIES {
                    return self.escalate_failed_retry("F10", sid, fid);
                }
                FailureResult {
                    action: FailureAction::Retry,
                    failure_mode: Some("F10".to_owned()),
                    plain_language: msg,
                    is_recoverable: true,
                    severity: FailureSeverity::Suggest,
                    step_id: sid,
                    focus_id: fid,
                    metadata: None,
                }
            }

            // F10 — Timeout / unavailable: retry with require severity
            ConductorError::ProviderTimeout { .. }
            | ConductorError::ProviderUnavailable { .. } => {
                if retry_count >= MAX_RETRIES {
                    return self.escalate_failed_retry("F10", sid, fid);
                }
                FailureResult {
                    action: FailureAction::Retry,
                    failure_mode: Some("F10".to_owned()),
                    plain_language: msg,
                    is_recoverable: true,
                    severity: FailureSeverity::Require,
                    step_id: sid,
                    focus_id: fid,
                    metadata: None,
                }
            }

            // F10 — Generic provider error
            ConductorError::Provider { .. } => FailureResult {
                action: FailureAction::AwaitUser,
                failure_mode: Some("F10".to_owned()),
                plain_language: msg,
                is_recoverable: true,
                severity: FailureSeverity::Require,
                step_id: sid,
                focus_id: fid,
                metadata: None,
            },
        }
        // Match is exhaustive over ConductorError variants.
        // F_UNEXPECTED is produced via handle_unexpected() for non-ConductorError errors.
    }

    /// Produce an F_UNEXPECTED result for errors that cannot be classified
    /// as a known ConductorError variant. Called by StepExecutor for
    /// non-ConductorError panics / unknown errors.
    /// Python oracle: the final fallthrough in FailureHandler.handle()
    pub fn handle_unexpected(
        &self,
        step_id: Option<&str>,
        focus_id: Option<&str>,
    ) -> FailureResult {
        FailureResult {
            action: FailureAction::Stop,
            failure_mode: Some("F_UNEXPECTED".to_owned()),
            plain_language: concat!(
                "Something unexpected happened. Your work is saved. ",
                "[Try again] [Get help]",
            ).to_owned(),
            is_recoverable: false,
            severity: FailureSeverity::Stop,
            step_id: step_id.map(|s| s.to_owned()),
            focus_id: focus_id.map(|s| s.to_owned()),
            metadata: None,
        }
    }

    // -- Private helpers -----------------------------------------------------

    fn handle_f1_unavailable(
        &self,
        msg: String,
        step_id: Option<String>,
        focus_id: Option<String>,
    ) -> FailureResult {
        if self.space_max_permitted_tier >= 2 {
            return FailureResult {
                action: FailureAction::OfferTier2,
                failure_mode: Some("F1".to_owned()),
                plain_language: msg,
                is_recoverable: true,
                severity: FailureSeverity::Require,
                step_id,
                focus_id,
                metadata: None,
            };
        }
        FailureResult {
            action: FailureAction::Stop,
            failure_mode: Some("F1".to_owned()),
            plain_language: concat!(
                "The local AI isn't responding, and this life doesn't ",
                "allow external services. [Try again] [Get help]",
            ).to_owned(),
            is_recoverable: false,
            severity: FailureSeverity::Stop,
            step_id,
            focus_id,
            metadata: None,
        }
    }

    fn handle_f2(
        &self,
        _msg: String,
        step_id: Option<String>,
        focus_id: Option<String>,
        retry_count: u32,
    ) -> FailureResult {
        let exhausted = retry_count >= MAX_RETRIES;
        if self.space_max_permitted_tier >= 2 {
            return FailureResult {
                action: FailureAction::OfferTier2,
                failure_mode: Some("F2".to_owned()),
                plain_language: if exhausted {
                    concat!(
                        "The result quality fell below standard repeatedly. ",
                        "Try using an external service? ",
                        "[Use external service] [Keep result] [Get help]",
                    ).to_owned()
                } else {
                    concat!(
                        "The result wasn't quite right. ",
                        "Want to try with an external service? ",
                        "[Use external service] [Keep this result] [Try again]",
                    ).to_owned()
                },
                is_recoverable: true,
                severity: if exhausted { FailureSeverity::Require } else { FailureSeverity::Suggest },
                step_id,
                focus_id,
                metadata: None,
            };
        }
        FailureResult {
            action: if exhausted { FailureAction::AwaitUser } else { FailureAction::Retry },
            failure_mode: Some("F2".to_owned()),
            plain_language: if exhausted {
                concat!(
                    "The local model output quality fell below standard repeatedly. ",
                    "[Review output] [Try again]",
                ).to_owned()
            } else {
                "The result wasn't quite right. Trying again. [Keep this result]".to_owned()
            },
            is_recoverable: true,
            severity: if exhausted { FailureSeverity::Require } else { FailureSeverity::Suggest },
            step_id,
            focus_id,
            metadata: None,
        }
    }

    fn escalate_failed_retry(
        &self,
        mode: &str,
        step_id: Option<String>,
        focus_id: Option<String>,
    ) -> FailureResult {
        if self.space_max_permitted_tier >= 2 {
            return FailureResult {
                action: FailureAction::OfferTier2,
                failure_mode: Some(mode.to_owned()),
                plain_language: concat!(
                    "This step has failed repeatedly. ",
                    "Switch to an external service? [Use external service] [Stop]",
                ).to_owned(),
                is_recoverable: true,
                severity: FailureSeverity::Require,
                step_id,
                focus_id,
                metadata: None,
            };
        }
        FailureResult {
            action: FailureAction::AwaitUser,
            failure_mode: Some(mode.to_owned()),
            plain_language: concat!(
                "This step failed repeatedly and has been paused. ",
                "[Try again] [Get help]",
            ).to_owned(),
            is_recoverable: true,
            severity: FailureSeverity::Stop,
            step_id,
            focus_id,
            metadata: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn handler(tier: u8) -> FailureHandler {
        FailureHandler::new(tier)
    }

    fn err(variant: ConductorError) -> ConductorError {
        variant
    }

    // -- F_SYSTEM ------------------------------------------------------------

    #[test]
    fn f_system_taxonomy_integrity() {
        let h = handler(1);
        let r = h.handle(
            &err(ConductorError::TaxonomyIntegrity { plain_language: "bad".to_owned() }),
            None, None, 0,
        );
        assert_eq!(r.action, FailureAction::Stop);
        assert_eq!(r.failure_mode.as_deref(), Some("F_SYSTEM"));
        assert!(!r.is_recoverable);
        assert_eq!(r.severity, FailureSeverity::Stop);
    }

    #[test]
    fn f_system_disclosure_log_write() {
        let h = handler(1);
        let r = h.handle(
            &err(ConductorError::DisclosureLogWrite { plain_language: "audit fail".to_owned() }),
            Some("s1"), Some("f1"), 0,
        );
        assert_eq!(r.failure_mode.as_deref(), Some("F_SYSTEM"));
        assert!(!r.is_recoverable);
        assert_eq!(r.step_id.as_deref(), Some("s1"));
        assert_eq!(r.focus_id.as_deref(), Some("f1"));
    }

    // -- F1 ------------------------------------------------------------------

    #[test]
    fn f1_unavailable_tier1_stops() {
        let h = handler(1);
        let r = h.handle(
            &err(ConductorError::OllamaUnavailable { plain_language: "down".to_owned() }),
            None, None, 0,
        );
        assert_eq!(r.action, FailureAction::Stop);
        assert!(!r.is_recoverable);
        assert!(r.plain_language.contains("doesn't allow external services"));
    }

    #[test]
    fn f1_unavailable_tier2_offers_tier2() {
        let h = handler(2);
        let r = h.handle(
            &err(ConductorError::OllamaUnavailable { plain_language: "down".to_owned() }),
            None, None, 0,
        );
        assert_eq!(r.action, FailureAction::OfferTier2);
        assert_eq!(r.failure_mode.as_deref(), Some("F1"));
    }

    #[test]
    fn f1_timeout_retries_under_max() {
        let h = handler(1);
        let r = h.handle(
            &err(ConductorError::OllamaTimeout { plain_language: "timeout".to_owned() }),
            None, None, 2,
        );
        assert_eq!(r.action, FailureAction::Retry);
    }

    #[test]
    fn f1_timeout_escalates_at_max_tier1() {
        let h = handler(1);
        let r = h.handle(
            &err(ConductorError::OllamaTimeout { plain_language: "timeout".to_owned() }),
            None, None, MAX_RETRIES,
        );
        assert_eq!(r.action, FailureAction::AwaitUser);
        assert_eq!(r.failure_mode.as_deref(), Some("F1"));
    }

    #[test]
    fn f1_timeout_escalates_at_max_tier2() {
        let h = handler(2);
        let r = h.handle(
            &err(ConductorError::OllamaTimeout { plain_language: "timeout".to_owned() }),
            None, None, MAX_RETRIES,
        );
        assert_eq!(r.action, FailureAction::OfferTier2);
    }

    #[test]
    fn f1_invalid_request_stops() {
        let h = handler(2);
        let r = h.handle(
            &err(ConductorError::OllamaInvalidRequest { plain_language: "bad req".to_owned() }),
            None, None, 0,
        );
        assert_eq!(r.action, FailureAction::Stop);
        assert!(!r.is_recoverable);
    }

    // -- F2 ------------------------------------------------------------------

    #[test]
    fn f2_first_attempt_tier1_retries() {
        let h = handler(1);
        let r = h.handle(
            &err(ConductorError::QualityBelowFloor { plain_language: "low".to_owned() }),
            None, None, 0,
        );
        assert_eq!(r.action, FailureAction::Retry);
        assert_eq!(r.severity, FailureSeverity::Suggest);
    }

    #[test]
    fn f2_exhausted_tier1_awaits_user() {
        let h = handler(1);
        let r = h.handle(
            &err(ConductorError::QualityBelowFloor { plain_language: "low".to_owned() }),
            None, None, MAX_RETRIES,
        );
        assert_eq!(r.action, FailureAction::AwaitUser);
        assert_eq!(r.severity, FailureSeverity::Require);
    }

    #[test]
    fn f2_first_attempt_tier2_offers_tier2() {
        let h = handler(2);
        let r = h.handle(
            &err(ConductorError::QualityBelowFloor { plain_language: "low".to_owned() }),
            None, None, 0,
        );
        assert_eq!(r.action, FailureAction::OfferTier2);
        assert_eq!(r.severity, FailureSeverity::Suggest);
    }

    #[test]
    fn f2_exhausted_tier2_offers_tier2_require() {
        let h = handler(2);
        let r = h.handle(
            &err(ConductorError::QualityBelowFloor { plain_language: "low".to_owned() }),
            None, None, MAX_RETRIES,
        );
        assert_eq!(r.action, FailureAction::OfferTier2);
        assert_eq!(r.severity, FailureSeverity::Require);
    }

    // -- F3-F9 ---------------------------------------------------------------

    #[test]
    fn f3_context_window_offers_compact() {
        let h = handler(1);
        let r = h.handle(
            &err(ConductorError::ContextWindowExceeded { plain_language: "too long".to_owned() }),
            None, None, 0,
        );
        assert_eq!(r.action, FailureAction::OfferCompact);
        assert_eq!(r.failure_mode.as_deref(), Some("F3"));
    }

    #[test]
    fn f4_privacy_gate_blocked_awaits_user() {
        let h = handler(1);
        let r = h.handle(
            &err(ConductorError::PrivacyGateBlocked { plain_language: "blocked".to_owned() }),
            None, None, 0,
        );
        assert_eq!(r.action, FailureAction::AwaitUser);
        assert_eq!(r.severity, FailureSeverity::Stop);
    }

    #[test]
    fn f4_content_promotion_blocked_awaits_user() {
        let h = handler(1);
        let r = h.handle(
            &err(ConductorError::ContentPromotionBlocked { plain_language: "blocked".to_owned() }),
            None, None, 0,
        );
        assert_eq!(r.action, FailureAction::AwaitUser);
        assert_eq!(r.failure_mode.as_deref(), Some("F4"));
    }

    #[test]
    fn f5_security_flag_stops() {
        let h = handler(1);
        let r = h.handle(
            &err(ConductorError::SecurityCheckerFlag { plain_language: "flagged".to_owned() }),
            None, None, 0,
        );
        assert_eq!(r.action, FailureAction::Stop);
        assert!(!r.is_recoverable);
    }

    #[test]
    fn f6_inbound_contamination_holds_for_gate() {
        let h = handler(1);
        let r = h.handle(
            &err(ConductorError::InboundContamination { plain_language: "contaminated".to_owned() }),
            None, None, 0,
        );
        assert_eq!(r.action, FailureAction::HoldForGate);
        assert_eq!(r.failure_mode.as_deref(), Some("F6"));
    }

    #[test]
    fn f7_personal_db_not_found_stops() {
        let h = handler(1);
        let r = h.handle(
            &err(ConductorError::PersonalDbNotFound { plain_language: "missing".to_owned() }),
            None, None, 0,
        );
        assert_eq!(r.action, FailureAction::Stop);
        assert!(!r.is_recoverable);
        assert_eq!(r.failure_mode.as_deref(), Some("F7"));
    }

    #[test]
    fn f8_snapshot_write_degrades() {
        let h = handler(1);
        let r = h.handle(
            &err(ConductorError::SnapshotWrite { plain_language: "disk full".to_owned() }),
            None, None, 0,
        );
        assert_eq!(r.action, FailureAction::Degrade);
        assert_eq!(r.severity, FailureSeverity::Suggest);
        assert!(r.plain_language.contains("progress checkpoint"));
    }

    #[test]
    fn f9_loop_detected_stops() {
        let h = handler(1);
        let r = h.handle(
            &err(ConductorError::LoopDetected { plain_language: "loop".to_owned() }),
            None, None, 0,
        );
        assert_eq!(r.action, FailureAction::Stop);
        assert!(!r.is_recoverable);
        assert!(r.plain_language.contains("detected a loop"));
    }

    // -- F10 -----------------------------------------------------------------

    #[test]
    fn f10_missing_api_key_awaits_user() {
        let h = handler(2);
        let r = h.handle(
            &err(ConductorError::MissingApiKey { plain_language: "no key".to_owned() }),
            None, None, 0,
        );
        assert_eq!(r.action, FailureAction::AwaitUser);
        assert_eq!(r.failure_mode.as_deref(), Some("F10"));
    }

    #[test]
    fn f10_rate_limit_retries_with_suggest() {
        let h = handler(2);
        let r = h.handle(
            &err(ConductorError::ProviderRateLimit { plain_language: "429".to_owned() }),
            None, None, 0,
        );
        assert_eq!(r.action, FailureAction::Retry);
        assert_eq!(r.severity, FailureSeverity::Suggest);
    }

    #[test]
    fn f10_rate_limit_escalates_at_max() {
        let h = handler(2);
        let r = h.handle(
            &err(ConductorError::ProviderRateLimit { plain_language: "429".to_owned() }),
            None, None, MAX_RETRIES,
        );
        assert_eq!(r.action, FailureAction::OfferTier2);
    }

    #[test]
    fn f10_provider_timeout_retries() {
        let h = handler(1);
        let r = h.handle(
            &err(ConductorError::ProviderTimeout { plain_language: "timeout".to_owned() }),
            None, None, 1,
        );
        assert_eq!(r.action, FailureAction::Retry);
        assert_eq!(r.severity, FailureSeverity::Require);
    }

    #[test]
    fn f10_generic_provider_awaits_user() {
        let h = handler(2);
        let r = h.handle(
            &err(ConductorError::Provider { plain_language: "oops".to_owned() }),
            None, None, 0,
        );
        assert_eq!(r.action, FailureAction::AwaitUser);
        assert_eq!(r.failure_mode.as_deref(), Some("F10"));
    }

    // -- handle_unexpected ---------------------------------------------------

    #[test]
    fn handle_unexpected_stops_non_recoverable() {
        let h = handler(1);
        let r = h.handle_unexpected(Some("s1"), Some("f1"));
        assert_eq!(r.action, FailureAction::Stop);
        assert_eq!(r.failure_mode.as_deref(), Some("F_UNEXPECTED"));
        assert!(!r.is_recoverable);
        assert!(r.plain_language.contains("unexpected happened"));
        assert_eq!(r.step_id.as_deref(), Some("s1"));
        assert_eq!(r.focus_id.as_deref(), Some("f1"));
    }

    // -- step_id / focus_id threading ----------------------------------------

    #[test]
    fn step_and_focus_id_threaded_through() {
        let h = handler(1);
        let r = h.handle(
            &err(ConductorError::LoopDetected { plain_language: "loop".to_owned() }),
            Some("step-42"), Some("focus-7"), 0,
        );
        assert_eq!(r.step_id.as_deref(), Some("step-42"));
        assert_eq!(r.focus_id.as_deref(), Some("focus-7"));
    }
}
