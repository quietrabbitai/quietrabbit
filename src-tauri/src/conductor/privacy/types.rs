// src-tauri/src/conductor/privacy/types.rs

use indexmap::IndexMap;
use serde::Deserialize;

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
    pub fn from_str(s: &str) -> Self {
        match s {
            "pass"          => Self::Pass,
            "omit"          => Self::Omit,
            "summarize"     => Self::Summarize,
            "range_only"    => Self::RangeOnly,
            "not_permitted" => Self::NotPermitted,
            other           => Self::Unknown(other.to_string()),
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
            Sensitivity::General   => 1,
            Sensitivity::Personal  => 2,
            Sensitivity::Medical   => 3,
            Sensitivity::Financial => 4,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Sensitivity::General   => "general",
            Sensitivity::Personal  => "personal",
            Sensitivity::Medical   => "medical",
            Sensitivity::Financial => "financial",
        }
    }
}

// -- PersonalField ------------------------------------------------------------
// Mirrors Python's PersonalField dataclass.
// field_value is decrypted plaintext -- never logged, never serialised.

#[derive(Debug, Clone)]
pub struct PersonalField {
    pub field_name:           String,
    pub field_value:          String,
    pub sensitivity:          Sensitivity,
    pub sensitivity_severity: u8,
    pub source_id:            String,
    pub abstraction_tier2:    AbstractionPolicy,
    pub abstraction_tier3:    AbstractionPolicy,
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
    pub approved_fields:      IndexMap<String, String>,
    pub withheld_fields:      Vec<String>,
    pub fields_shared:        Vec<String>,
    pub floor_clamped_fields: Vec<String>,
    pub disclosure_log_id:    String,
    pub blocked:              bool,
}

#[derive(Debug)]
pub struct Gate2Result {
    pub flagged:             bool,
    pub matched_field_names: Vec<String>,
}

#[derive(Debug)]
pub struct Gate3Result {
    pub approved:       bool,
    pub blocked:        bool,
    pub plain_language: Option<String>,
}

#[derive(Debug)]
pub struct Gate4Result {
    pub content_approved:  bool,
    pub clipboard_blocked: bool,
    pub plain_language:    Option<String>,
}

pub const CLIPBOARD_MAX_SENSITIVITY_SEVERITY: u8 = 2;
