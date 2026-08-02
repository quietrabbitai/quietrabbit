// src-tauri/src/conductor/privacy/output_scan.rs
//
// cb-09 -- Output Privacy Guardian scan.
// Foundation block, items.id=128 / decisions.id=621 (catalog_status
// 'confirmed'). BUILD STATUS 2026-07-25 (Chat-DEV full-catalog survey,
// handoff id=103): confirmed a true gap -- every existing "Privacy
// Guardian" reference was either Gate3's mid-execution cross-tier
// promotion consent, or store/IPC infrastructure built FOR a future
// Privacy Guardian to consume, but no post-hoc scan of finalized output
// before write/download/export existed anywhere. This module is that scan.
// Consistent with decisions.id=546's own framing of this as a known,
// expected, not-yet-built component.
//
// RELATIONSHIP TO GATE3: this is a distinct trigger point, not a
// duplicate. Gate3 (gate3.rs) fires MID-execution, when content is about
// to cross a tier boundary during a run. This scan fires POST-generation,
// on finalized output immediately before a write/download/export
// boundary crossing -- it may run even when no tier promotion is
// involved at all (e.g. writing a local-only Tier 1 result to disk).
// Per project_guide: privacy_guardian_scope, "Privacy Guardian is an
// egress gate, not a retention/curation manager -- fires when content is
// about to cross a tier boundary, never on data-at-rest" -- write/
// download/export IS an egress boundary (app -> filesystem/clipboard/
// external destination), so this scan is in-scope under that same
// framing, just a different boundary than Gate3's tier-to-tier one.
//
// REUSE, NOT DUPLICATION (P4): detection is delegated entirely to the
// existing privacy_filter FFI module (privacy_filter::run_classify_blocking),
// the same engine gate3.rs already calls for its Privacy Filter path. This
// module does not reimplement entity detection -- it only decides what to
// do with the spans returned (light vs full intensity) and writes the
// disclosure log entry via the existing DisclosureLogger trait, exactly as
// every other gate does.
//
// TWO INTENSITY SETTINGS (cb-09 description -- confirmed NOT two separate
// blocks, a single parameter on one function):
//   Light -- pre-validation checkpoint. Runs during composition/drafting,
//            before the user has committed to finalizing. Detected spans
//            are surfaced as warnings; nothing is blocked.
//   Full  -- final write/export boundary. Detected spans above the
//            severity threshold block the write outright, matching the
//            block description ("before write, download, or export").
//
// FFI AVAILABILITY: mirrors gate3.rs's own fallback discipline exactly --
// when privacy_filter::is_available() is false (PF not compiled in, e.g.
// dev/test), this module falls back to a conservative keyword/pattern-free
// pass-through: Full intensity still enforces the pre-existing severity-based
// legacy block (content_sensitivity_severity >= 3 blocks Full-intensity
// export), Light intensity never blocks. This is the same two-path
// discipline gate3.rs already established (PF path / legacy sensitivity
// fallback) -- not a new invented behavior.

use std::time::Duration;

use indexmap::IndexMap;

use super::{
    errors::DisclosureLogWriteError,
    logger::{DisclosureLogEntry, DisclosureLogger},
    privacy_filter::{self, PfEntityDecoded},
};

/// Matches gate3.rs's PF_TIMEOUT_SECS discipline -- same FFI call, same
/// timeout budget, so a scan does not hang the write/export path.
const PF_TIMEOUT_SECS: u64 = 5;

/// Legacy fallback threshold when PF is not available. Mirrors gate3.rs's
/// content_sensitivity_severity >= 3 sensitivity-block constant exactly --
/// see gate3.rs's "Check 2b: Legacy sensitivity block fallback" comment.
const LEGACY_BLOCK_SEVERITY: u8 = 3;

// ---------------------------------------------------------------------------
// ScanIntensity
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanIntensity {
    /// Pre-validation checkpoint -- surfaces warnings, never blocks.
    Light,
    /// Final write/export boundary -- blocks on detected sensitive spans.
    Full,
}

// ---------------------------------------------------------------------------
// OutputScanResult
// ---------------------------------------------------------------------------

/// One detected span, surfaced to the caller / UI. Deliberately narrower
/// than PfEntityDecoded -- byte offsets and label only, no raw span_text,
/// so a Light-intensity warning can be shown without echoing the sensitive
/// substring itself back through logs or IPC payloads unnecessarily.
#[derive(Debug, Clone)]
pub struct OutputScanFinding {
    pub start_byte: usize,
    pub end_byte: usize,
    pub label: String,
    pub score: f32,
}

#[derive(Debug, Clone)]
pub struct OutputScanResult {
    /// Full intensity only: true when the write/export must not proceed.
    /// Always false for Light intensity -- Light never blocks.
    pub blocked: bool,
    /// PF call exceeded PF_TIMEOUT_SECS. Full intensity treats a timeout
    /// as fail-closed (blocked=true) -- mirrors gate3.rs's own timeout
    /// handling, which also fails closed (blocked=true) on PF timeout.
    pub timed_out: bool,
    pub findings: Vec<OutputScanFinding>,
    pub plain_language: Option<String>,
}

// ---------------------------------------------------------------------------
// scan_output
// ---------------------------------------------------------------------------

/// Scan finalized output content immediately before a write, download, or
/// export boundary crossing. Always writes a disclosure log entry via the
/// injected logger, exactly as every other privacy gate does -- an
/// unlogged egress-boundary scan would be a silent audit gap.
///
/// step_id / focus_run_id / execution_tier are passed straight through to
/// DisclosureLogEntry, matching every other gate function's signature shape.
#[allow(clippy::too_many_arguments)] // Explicit architecture boundary; matches gate1-4's own allow.
pub async fn scan_output<L: DisclosureLogger>(
    logger: &L,
    step_id: &str,
    focus_run_id: &str,
    content_text: &str,
    execution_tier: u8,
    content_sensitivity_severity: u8,
    intensity: ScanIntensity,
) -> Result<OutputScanResult, DisclosureLogWriteError> {
    if privacy_filter::is_available() {
        return scan_with_pf(
            logger,
            step_id,
            focus_run_id,
            content_text,
            execution_tier,
            intensity,
        )
        .await;
    }

    scan_legacy_fallback(
        logger,
        step_id,
        focus_run_id,
        execution_tier,
        content_sensitivity_severity,
        intensity,
    )
    .await
}

// -- Privacy Filter path -----------------------------------------------------

async fn scan_with_pf<L: DisclosureLogger>(
    logger: &L,
    step_id: &str,
    focus_run_id: &str,
    content_text: &str,
    execution_tier: u8,
    intensity: ScanIntensity,
) -> Result<OutputScanResult, DisclosureLogWriteError> {
    let text = content_text.to_owned();

    let pf_task =
        tokio::task::spawn_blocking(move || privacy_filter::run_classify_blocking(&text, 0.0));

    let pf_outcome = tokio::time::timeout(Duration::from_secs(PF_TIMEOUT_SECS), pf_task).await;

    let entities: Vec<PfEntityDecoded> = match pf_outcome {
        Err(_elapsed) => {
            log::warn!("output_scan: Privacy Filter timed out after {PF_TIMEOUT_SECS}s");
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
                    override_declined: intensity == ScanIntensity::Full,
                    event_type: "output_scan_timeout".to_string(),
                })
                .await?;

            let blocked = intensity == ScanIntensity::Full;
            return Ok(OutputScanResult {
                blocked,
                timed_out: true,
                findings: vec![],
                plain_language: if blocked {
                    Some(
                        "The privacy check took too long to complete, so this \
                         export was blocked as a precaution. [Try again] [Get help]"
                            .to_string(),
                    )
                } else {
                    None
                },
            });
        }
        Ok(Ok(Ok(entities))) => entities,
        Ok(Ok(Err(e))) => {
            log::warn!("output_scan: Privacy Filter classify error: {e}");
            vec![]
        }
        Ok(Err(join_err)) => {
            log::warn!("output_scan: Privacy Filter task panicked: {join_err}");
            vec![]
        }
    };

    let findings: Vec<OutputScanFinding> = entities
        .iter()
        .map(|e| OutputScanFinding {
            start_byte: e.start_byte,
            end_byte: e.end_byte,
            label: e.label.clone(),
            score: e.score,
        })
        .collect();

    let has_findings = !findings.is_empty();
    let blocked = intensity == ScanIntensity::Full && has_findings;

    logger
        .write(DisclosureLogEntry {
            step_id: step_id.to_string(),
            focus_run_id: focus_run_id.to_string(),
            execution_tier,
            abstraction_tier: None,
            provider: None,
            fields_shared: vec![],
            fields_abstracted: IndexMap::new(),
            fields_withheld: findings.iter().map(|f| f.label.clone()).collect(),
            override_declined: blocked,
            event_type: match intensity {
                ScanIntensity::Light => "output_scan_light".to_string(),
                ScanIntensity::Full => "output_scan_full".to_string(),
            },
        })
        .await?;

    let plain_language = if blocked {
        Some(
            "This output appears to contain personal information and \
             can't be exported as-is. [Review and edit] [Get help]"
                .to_string(),
        )
    } else if has_findings {
        Some(
            "This draft may contain personal information -- worth a look \
             before you finish. [Review]"
                .to_string(),
        )
    } else {
        None
    };

    Ok(OutputScanResult {
        blocked,
        timed_out: false,
        findings,
        plain_language,
    })
}

// -- Legacy fallback path (PF not compiled in) -------------------------------

async fn scan_legacy_fallback<L: DisclosureLogger>(
    logger: &L,
    step_id: &str,
    focus_run_id: &str,
    execution_tier: u8,
    content_sensitivity_severity: u8,
    intensity: ScanIntensity,
) -> Result<OutputScanResult, DisclosureLogWriteError> {
    let blocked =
        intensity == ScanIntensity::Full && content_sensitivity_severity >= LEGACY_BLOCK_SEVERITY;

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
            override_declined: blocked,
            event_type: match intensity {
                ScanIntensity::Light => "output_scan_light_legacy".to_string(),
                ScanIntensity::Full => "output_scan_full_legacy".to_string(),
            },
        })
        .await?;

    let plain_language = if blocked {
        Some(
            "This content contains medical or financial information and \
             can't be exported. [Use local only] [Get help]"
                .to_string(),
        )
    } else {
        None
    };

    Ok(OutputScanResult {
        blocked,
        timed_out: false,
        findings: vec![],
        plain_language,
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------
//
// PF is not compiled in on this machine (PRIVACY_FILTER_LIB_DIR unset --
// see build warning), so privacy_filter::is_available() is always false in
// this test run and scan_output() always exercises scan_legacy_fallback().
// This mirrors gate3.rs's own test coverage split exactly (its PF-path
// tests are gated behind the same compiled-in condition; its legacy-path
// tests run unconditionally). No live-PF tests are added here for the same
// reason gate3.rs has none runnable on this machine.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conductor::privacy::logger::TestLogger;

    #[tokio::test]
    async fn legacy_light_never_blocks_even_at_max_severity() {
        let logger = TestLogger::new();
        let result = scan_output(
            &logger,
            "step-1",
            "run-1",
            "some finished draft text",
            1,
            4, // financial -- highest severity
            ScanIntensity::Light,
        )
        .await
        .unwrap();
        assert!(!result.blocked);
        assert!(!result.timed_out);
        assert_eq!(logger.entry_count(), 1);
        assert_eq!(logger.entries()[0].event_type, "output_scan_light_legacy");
    }

    #[tokio::test]
    async fn legacy_full_blocks_at_or_above_threshold() {
        let logger = TestLogger::new();
        let result = scan_output(
            &logger,
            "step-1",
            "run-1",
            "sensitive content",
            1,
            3,
            ScanIntensity::Full,
        )
        .await
        .unwrap();
        assert!(result.blocked);
        assert!(result.plain_language.is_some());
        assert_eq!(logger.entries()[0].event_type, "output_scan_full_legacy");
        assert!(logger.entries()[0].override_declined);
    }

    #[tokio::test]
    async fn legacy_full_below_threshold_does_not_block() {
        let logger = TestLogger::new();
        let result = scan_output(
            &logger,
            "step-1",
            "run-1",
            "ordinary content",
            1,
            2,
            ScanIntensity::Full,
        )
        .await
        .unwrap();
        assert!(!result.blocked);
        assert!(result.plain_language.is_none());
    }

    #[tokio::test]
    async fn legacy_fallback_never_produces_pf_findings() {
        // Without PF compiled in, findings must always be empty -- the
        // legacy path has no detection engine, only the severity threshold.
        let logger = TestLogger::new();
        let result = scan_output(
            &logger,
            "step-1",
            "run-1",
            "content",
            1,
            4,
            ScanIntensity::Full,
        )
        .await
        .unwrap();
        assert!(result.findings.is_empty());
    }

    #[tokio::test]
    async fn always_writes_disclosure_log_even_when_not_blocked() {
        let logger = TestLogger::new();
        let _ = scan_output(
            &logger,
            "step-1",
            "run-1",
            "content",
            1,
            1,
            ScanIntensity::Light,
        )
        .await
        .unwrap();
        assert_eq!(
            logger.entry_count(),
            1,
            "an unlogged egress-boundary scan would be a silent audit gap"
        );
    }

    #[tokio::test]
    async fn disclosure_log_write_failure_propagates() {
        use crate::conductor::privacy::logger::FailLogger;
        let logger = FailLogger;
        let result = scan_output(
            &logger,
            "step-1",
            "run-1",
            "content",
            1,
            1,
            ScanIntensity::Light,
        )
        .await;
        assert!(result.is_err());
    }
}
