// src-tauri/src/conductor/privacy/types.rs

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

// -- AbstractionPolicy --------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub enum AbstractionPolicy {
    Pass,
    Omit,
    Summarize,
    RangeOnly,
    NotPermitted,
    /// Invariant-violation variant: carries the raw unrecognised policy string.
    /// Used for golden-vector defensive tests and future deserialisation of
    /// corrupted DB rows. apply_abstraction() treats this as Omit (fail-closed).
    /// Never constructed in production code paths.
    Unknown(String),
}

impl AbstractionPolicy {
    #[allow(clippy::should_implement_trait)] // Returns Self not Result; cannot impl std::str::FromStr.
    pub fn from_str(s: &str) -> Self {
        match s {
            "pass" => Self::Pass,
            "omit" => Self::Omit,
            "summarize" => Self::Summarize,
            "range_only" => Self::RangeOnly,
            "not_permitted" => Self::NotPermitted,
            other => Self::Unknown(other.to_string()),
        }
    }
}

// -- Sensitivity --------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Sensitivity {
    General,
    Personal,
    Medical,
    Financial,
}

impl Sensitivity {
    pub fn severity(&self) -> u8 {
        match self {
            Sensitivity::General => 1,
            Sensitivity::Personal => 2,
            Sensitivity::Medical => 3,
            Sensitivity::Financial => 4,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Sensitivity::General => "general",
            Sensitivity::Personal => "personal",
            Sensitivity::Medical => "medical",
            Sensitivity::Financial => "financial",
        }
    }
}

// -- PersonalField ------------------------------------------------------------
// Mirrors Python's PersonalField dataclass.
// field_value is decrypted plaintext -- never logged, never serialised.

#[derive(Debug, Clone)]
pub struct PersonalField {
    pub field_name: String,
    pub field_value: String,
    pub sensitivity: Sensitivity,
    pub sensitivity_severity: u8,
    pub source_id: String,
    pub abstraction_tier2: AbstractionPolicy,
    pub abstraction_tier3: AbstractionPolicy,
}

// -- PersonalTrack ------------------------------------------------------------
// Mirrors Python's PersonalTrack. Sealed after INITIALIZE phase.
// IndexMap preserves insertion order -- required for approved_fields
// ordering parity with Python (dict preserves insertion order, 3.7+).

#[derive(Debug, Default)]
pub struct PersonalTrack {
    fields: IndexMap<String, PersonalField>,
    sealed: bool,
}

impl PersonalTrack {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_field(&mut self, field: PersonalField) -> Result<(), &'static str> {
        if self.sealed {
            return Err("PersonalTrack is sealed -- cannot modify after INITIALIZE");
        }
        self.fields.insert(field.field_name.clone(), field);
        Ok(())
    }

    pub fn seal(&mut self) {
        self.sealed = true;
    }

    pub fn is_sealed(&self) -> bool {
        self.sealed
    }

    pub fn fields(&self) -> &IndexMap<String, PersonalField> {
        &self.fields
    }

    pub fn is_empty(&self) -> bool {
        self.fields.is_empty()
    }
}

// -- Gate result types --------------------------------------------------------

#[derive(Debug)]
pub struct Gate1Result {
    pub approved_fields: IndexMap<String, String>,
    pub withheld_fields: Vec<String>,
    pub fields_shared: Vec<String>,
    pub floor_clamped_fields: Vec<String>,
    pub disclosure_log_id: String,
}

#[derive(Debug)]
pub struct Gate2Result {
    pub flagged: bool,
    pub matched_field_names: Vec<String>,
}

#[derive(Debug, Default)]
pub struct Gate3Result {
    pub approved: bool,
    pub blocked: bool,
    pub plain_language: Option<String>,
    /// Privacy Filter identified spans and emitted consent_request event.
    /// Execution is paused — waiting for per-element user decision.
    /// When true: approved=false, blocked=false.
    pub pending_consent: bool,
    /// Privacy Filter call exceeded the timeout window.
    /// gate_timeout event written to disclosure_log before returning.
    /// When true: approved=false, blocked=true.
    pub timeout: bool,
}

#[derive(Debug)]
pub struct Gate4Result {
    pub content_approved: bool,
    pub clipboard_blocked: bool,
    pub plain_language: Option<String>,
}

pub const CLIPBOARD_MAX_SENSITIVITY_SEVERITY: u8 = 2;

// -- Privacy Guardian Gate 3 IPC types ----------------------------------------
//
// These types cross the Tauri IPC boundary and must derive Serialize,
// Deserialize, and specta::Type as appropriate.
//
// ConsentRequestPayload / ConsentSpanItem / ReviewTier: emitted as the
// consent_request push event payload (Serialize + specta::Type).
//
// ElementDecision: received from the frontend per-element return command
// (Deserialize + specta::Type).

/// Review tier assigned by gate3 based on confidence scores, sensitivity, and
/// target execution tier. Determines the visual weight of the consent modal.
/// Serialises as "easy" | "medium" | "high".
#[derive(Debug, Clone, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "lowercase")]
pub enum ReviewTier {
    /// High-confidence structural PII (score >= 0.90). Minimal friction.
    Easy,
    /// Moderate confidence or contextual content (score >= 0.70).
    Medium,
    /// Low confidence, Medical-mapped category, or Tier 3 target.
    /// Err toward High when uncertain (D6-362).
    High,
}

/// A single identified span within the consent_request payload.
/// One item per entity returned by the Privacy Filter.
#[derive(Debug, Clone, Serialize, specta::Type)]
pub struct ConsentSpanItem {
    /// Unique ID within this gate invocation. Used to correlate ElementDecision
    /// responses from the frontend back to the original span.
    pub span_id: String,
    /// Raw category label from the Privacy Filter (e.g. "private_person").
    pub category: String,
    /// Human-readable label for display (e.g. "Person name").
    /// Derived from the QR taxonomy mapping in gate3.rs.
    pub user_label: String,
    /// The original text identified by the Privacy Filter.
    pub original_text: String,
    /// Pre-populated generalization suggestion (e.g. "[person]").
    /// None if no rule matches — frontend renders an editable placeholder.
    pub suggestion: Option<String>,
    /// Byte offset of the span start in the original content text (inclusive).
    /// Slicing must use byte indexing: &text[start_byte..end_byte].
    pub start_byte: usize,
    /// Byte offset of the span end in the original content text (exclusive).
    pub end_byte: usize,
    /// Confidence score from the Privacy Filter in [0.0, 1.0].
    pub score: f32,
}

/// Full payload for the consent_request push event emitted by gate3.
/// Received by the frontend to populate the Privacy Guardian modal.
#[derive(Debug, Clone, Serialize, specta::Type)]
pub struct ConsentRequestPayload {
    /// ID of the paused FocusRun — used by the frontend to route the
    /// per-element return command back to the correct run.
    pub focus_run_id: String,
    /// Display name of the Focus being executed (shown in the modal header).
    pub focus_name: String,
    /// Review tier for the modal. Controls visual weight and friction level.
    pub review_tier: ReviewTier,
    /// Identified spans. May be empty — see gate3.rs zero-span handling.
    pub spans: Vec<ConsentSpanItem>,
}

/// The user's decision type for a single identified span.
/// Deserialises from "generalize" | "keep_private" | "release_original".
#[derive(Debug, Clone, Deserialize, specta::Type)]
#[serde(rename_all = "snake_case")]
pub enum ElementDecisionKind {
    /// Replace the identified span with the generalisation suggestion.
    Generalize,
    /// Remove the span from content entirely before sending.
    KeepPrivate,
    /// Send the original identified text as-is.
    ReleaseOriginal,
}

/// Per-element decision returned from the frontend after the user reviews
/// the Privacy Guardian modal. One ElementDecision per ConsentSpanItem.
#[derive(Debug, Deserialize, specta::Type)]
pub struct ElementDecision {
    /// Matches ConsentSpanItem.span_id for this decision.
    pub span_id: String,
    /// The user's choice for this span.
    pub decision: ElementDecisionKind,
    /// The suggestion text that was displayed to the user (original, unedited).
    /// Required per IPC flag: per-element return must include original suggestion.
    pub suggestion_text: Option<String>,
    /// The text the user actually entered, if they edited the suggestion.
    /// None if the user accepted the suggestion without modification.
    pub user_modified_text: Option<String>,
}

// -- Extract-and-confirm IPC types --------------------------------------------
//
// These types cross the Tauri IPC boundary for the extract-and-confirm flow
// (item 20). Both derive specta::Type for TypeScript generation.
//
// ExtractedCandidate: emitted in the extract_confirm_request push event payload.
// ExtractConfirmDecision: received per candidate in submit_extract_confirm.

/// A single extraction candidate surfaced to the frontend for confirmation.
/// Emitted as part of the extract_confirm_request push event payload.
/// candidate_id is i64 (INTEGER PRIMARY KEY in extract_confirm_candidates).
#[derive(Debug, Clone, Serialize, specta::Type)]
pub struct ExtractedCandidate {
    /// Row id in extract_confirm_candidates. i64 matches INTEGER PRIMARY KEY.
    pub candidate_id: i64,
    pub field_name: String,
    pub extracted_value: String,
    /// Sensitivity tier: "medical" | "financial" | "personal".
    pub sensitivity: String,
    /// One sentence explaining why this fact was extracted.
    pub reason: Option<String>,
    /// Confidence in [0.6, 1.0] -- values below 0.6 are suppressed pre-persist.
    pub confidence: f64,
    /// True when confidence is in the 0.6-0.8 warn band.
    /// Frontend should surface a lower-confidence indicator for these candidates.
    pub warn_flag: bool,
}

/// The user's decision for a single extraction candidate.
/// Received from the frontend via submit_extract_confirm.
///
/// confirmed_value must be Some(String) when confirmed == true.
/// Violation of this invariant is treated as a command validation error:
/// submit_extract_confirm rejects the entire call before any DB mutation.
///
/// extracted_value is included for audit provenance -- preserved alongside
/// confirmed_value so the original extraction is never lost even when the
/// user edits before confirming.
#[derive(Debug, Clone, Deserialize, specta::Type)]
pub struct ExtractConfirmDecision {
    /// Matches ExtractedCandidate.candidate_id.
    pub candidate_id: i64,
    /// True if the user accepted this candidate for storage.
    pub confirmed: bool,
    /// The original extracted value (audit provenance -- always required).
    pub extracted_value: String,
    /// The value the user accepted, possibly edited. Must be Some when confirmed == true.
    pub confirmed_value: Option<String>,
}
