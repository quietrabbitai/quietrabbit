// src-tauri/src/conductor/model_capability.rs
//
// items.id=896 stage 1 (decisions.id=942 point 9 and rulings Q12, addendum 3 b
// and c): what a model has been MEASURED to do, per task type and document
// profile. This is data the resolver (stage 2) reads; nothing here chooses a
// model.
//
// ONE ROW = one measured cell: (subject, provider, model, task type, profile).
//   subject    local (an Ollama tag of a curated providers row) or hosted
//              (provider_id + model_id of a curated provider_models row).
//   task_type  ModelTaskType, the closed vocabulary of model_block::config.
//   profile    the id of a SHIPPED document type ("rma_letter", "cover_letter",
//              ...), or "" for a task-type-wide cell. Calibration found that
//              strength varies by kind of document, not by model size, and every
//              writing document type has the one task type creative_writing, so
//              the task type alone would hide the variation (items.id=896 plan,
//              approved with this key). A custom document type names a shipped
//              type as its base (DocumentType::based_on), so its profile is the
//              base's: no custom type ever needs its own row.
//   attained_bar  the highest named quality bar the model reached on that cell:
//              below_basic (measured, not adequate), basic, standard, high. The
//              resolver compares it with QualityBar. An ordinal bar, not a number:
//              Jason's read of the first calibration is ordinal, and the numeric
//              scores of the full harness (items.id=899) move the bar thresholds,
//              not this column.
//   model_class   small, medium or large: a property of the model repeated on
//              each of its rows (the apply step refuses two classes for one
//              model). The resolver's ranking hint, never a filter.
//   provenance jason_read (a human read of saved outputs), harness_local or
//              harness_hosted.
//
// A CELL THAT IS NOT HERE IS UNMEASURED, which is not the same as below_basic.
// The resolver never chooses an unmeasured model automatically (items.id=896
// plan point 4); it lists it as "not measured" among the options.
//
// WHO WRITES THE ROWS. Only a signed catalog (provider_catalog::apply) and the
// seed of shared_045.sql, which is the baseline catalog's own capabilities
// section. Nothing at run time writes this table.

use serde::{Deserialize, Serialize};

use crate::conductor::model_block::config::{ModelClass, ModelTaskType, QualityBar};

/// Where a model runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilitySubject {
    Local,
    Hosted,
}

impl CapabilitySubject {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Hosted => "hosted",
        }
    }
}

/// The highest named bar a model reached on one cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttainedBar {
    BelowBasic,
    Basic,
    Standard,
    High,
}

impl AttainedBar {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::BelowBasic => "below_basic",
            Self::Basic => "basic",
            Self::Standard => "standard",
            Self::High => "high",
        }
    }

    /// Whether this attainment satisfies the required bar. below_basic meets
    /// none.
    pub fn meets(self, required: QualityBar) -> bool {
        let rank = |b: AttainedBar| match b {
            AttainedBar::BelowBasic => 0u8,
            AttainedBar::Basic => 1,
            AttainedBar::Standard => 2,
            AttainedBar::High => 3,
        };
        let need = match required {
            QualityBar::Basic => 1u8,
            QualityBar::Standard => 2,
            QualityBar::High => 3,
        };
        rank(self) >= need
    }
}

/// How a cell's attainment was established.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Provenance {
    /// A human read of saved outputs (decisions.id=942 addendum 3 b).
    JasonRead,
    /// The local half of the calibration harness (items.id=899).
    HarnessLocal,
    /// The hosted half (items.id=788 and 726).
    HarnessHosted,
}

impl Provenance {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::JasonRead => "jason_read",
            Self::HarnessLocal => "harness_local",
            Self::HarnessHosted => "harness_hosted",
        }
    }
}

/// The text form of a ModelClass, as the table stores it.
pub fn class_str(c: ModelClass) -> &'static str {
    match c {
        ModelClass::Small => "small",
        ModelClass::Medium => "medium",
        ModelClass::Large => "large",
    }
}

/// One measured cell, as read back from the table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapabilityRow {
    pub subject: CapabilitySubject,
    /// Empty for a local model.
    pub provider_id: String,
    /// A local model's Ollama tag, or a hosted model's id.
    pub model_id: String,
    pub task_type: ModelTaskType,
    /// A shipped document type id, or empty for a task-type-wide cell.
    pub profile: String,
    pub attained_bar: AttainedBar,
    pub model_class: ModelClass,
    pub sample_count: u32,
    pub measured_at: String,
    pub provenance: Provenance,
    pub note: Option<String>,
}

/// Parse one of the table's closed text vocabularies. None for a value this
/// build does not know, so a reader skips the row instead of guessing.
pub fn parse_vocab<T: serde::de::DeserializeOwned>(text: &str) -> Option<T> {
    serde_json::from_value(serde_json::Value::String(text.to_owned())).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_attained_bar_meets_exactly_the_bars_at_or_below_it() {
        use QualityBar::*;
        let table = [
            (AttainedBar::BelowBasic, [false, false, false]),
            (AttainedBar::Basic, [true, false, false]),
            (AttainedBar::Standard, [true, true, false]),
            (AttainedBar::High, [true, true, true]),
        ];
        for (attained, expect) in table {
            for (bar, want) in [Basic, Standard, High].into_iter().zip(expect) {
                assert_eq!(attained.meets(bar), want, "{attained:?} vs {bar:?}");
            }
        }
    }

    #[test]
    fn the_ordering_agrees_with_meets() {
        assert!(AttainedBar::BelowBasic < AttainedBar::Basic);
        assert!(AttainedBar::Basic < AttainedBar::Standard);
        assert!(AttainedBar::Standard < AttainedBar::High);
    }

    #[test]
    fn every_vocabulary_round_trips_through_its_text_form() {
        for b in [
            AttainedBar::BelowBasic,
            AttainedBar::Basic,
            AttainedBar::Standard,
            AttainedBar::High,
        ] {
            assert_eq!(parse_vocab::<AttainedBar>(b.as_str()), Some(b));
        }
        for p in [
            Provenance::JasonRead,
            Provenance::HarnessLocal,
            Provenance::HarnessHosted,
        ] {
            assert_eq!(parse_vocab::<Provenance>(p.as_str()), Some(p));
        }
        for s in [CapabilitySubject::Local, CapabilitySubject::Hosted] {
            assert_eq!(parse_vocab::<CapabilitySubject>(s.as_str()), Some(s));
        }
        for c in [ModelClass::Small, ModelClass::Medium, ModelClass::Large] {
            assert_eq!(parse_vocab::<ModelClass>(class_str(c)), Some(c));
        }
        assert_eq!(parse_vocab::<AttainedBar>("excellent"), None);
        assert_eq!(parse_vocab::<ModelTaskType>("poetry"), None);
    }
}
