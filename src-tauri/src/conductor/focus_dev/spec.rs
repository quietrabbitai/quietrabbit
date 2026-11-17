// src-tauri/src/conductor/focus_dev/spec.rs
//
// The Focus spec: the source of truth a Focus is seeded from (decisions.id=951 ruling 1).
// Every struct is deny_unknown_fields, so a typo is an error and never a silently dropped
// key. A spec states the focuses row (its one home for display name and description),
// ordered rows (an ordinary step, or a block citing its configuration), typed guards,
// store-writer columns, a schedule trigger, and the launch questions.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::conductor::focus_dev::preset::{self, PresetFile};
use crate::conductor::focus_dev::FocusDevError;
use crate::persistence::focus_composition_store::{
    CompositionSeed, FocusSeed, FocusSeedRow, LaunchOptionSeed, LaunchQuestionSeed,
};

pub const SPEC_FORMAT_VERSION: u32 = 1;

fn default_version() -> String {
    "1.0".to_owned()
}
fn default_output_type() -> String {
    "general".to_owned()
}
fn default_title_template() -> String {
    "Hidden item".to_owned()
}
fn default_posture() -> String {
    "per_field".to_owned()
}
fn yes() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FocusSpec {
    pub format_version: u32,
    pub focus: SpecFocus,
    pub rows: Vec<SpecEntry>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub launch: Vec<SpecQuestion>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ui_one_liner: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SpecFocus {
    pub focus_id: String,
    pub display_name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default = "default_version")]
    pub version: String,
    pub max_routing_tier: String,
    #[serde(default = "default_output_type")]
    pub output_type: String,
    #[serde(default)]
    pub suggest_in_focuses: Vec<String>,
    #[serde(default)]
    pub multi_source_validation: bool,
    #[serde(default = "default_title_template")]
    pub generic_title_template: String,
    #[serde(default)]
    pub high_priority_trigger: Option<SpecTrigger>,
    pub status: String,
    #[serde(default = "default_posture")]
    pub collection_posture: String,
}

/// A trigger (high-priority at Focus level, schedule at row level): an anchor field and an offset.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SpecTrigger {
    pub anchor_field: String,
    pub offset: String,
}

/// One typed guard, the same shape as a guard_clauses element (decisions.id=938):
/// var, op (eq, ne, in, present, absent) and a value that is a string (eq, ne), an
/// array of strings (in) or absent (present, absent).
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SpecGuard {
    pub var: String,
    pub op: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SpecStore {
    pub op: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// A JSON object of slot to variable name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<Value>,
}

/// One composition row. `config` is the default customization JSON: an ordinary
/// step's fields, or a block's configuration.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SpecRow {
    pub action_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub block: Option<String>,
    #[serde(default = "empty_object")]
    pub config: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guard: Option<SpecGuard>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub guard_clauses: Vec<SpecGuard>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schedule_trigger: Option<SpecTrigger>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub store: Option<SpecStore>,
}

fn empty_object() -> Value {
    Value::Object(serde_json::Map::new())
}

/// A use of a shipped preset: `{"preset": id, "as": prefix, "params": {...}}`.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PresetUse {
    pub preset: String,
    #[serde(rename = "as")]
    pub prefix: String,
    #[serde(default)]
    pub params: BTreeMap<String, Value>,
}

/// An entry of `rows`: a row, or a preset use (told apart by the `preset` key).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum SpecEntry {
    Row(Box<SpecRow>),
    Preset(PresetUse),
}

impl<'de> Deserialize<'de> for SpecEntry {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        let v = Value::deserialize(d)?;
        if v.get("preset").is_some() {
            serde_json::from_value(v)
                .map(SpecEntry::Preset)
                .map_err(D::Error::custom)
        } else {
            serde_json::from_value::<SpecRow>(v)
                .map(|r| SpecEntry::Row(Box::new(r)))
                .map_err(D::Error::custom)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SpecQuestion {
    pub question_id: String,
    pub slot: String,
    pub kind: String,
    #[serde(default = "yes")]
    pub asked: bool,
    pub default_option: String,
    pub prompt: String,
    pub options: Vec<SpecOption>,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SpecOption {
    pub option_id: String,
    pub label: String,
    #[serde(default)]
    pub hint: String,
    /// A mode option's envelope override (a JSON object). None for a choice option.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub envelope: Option<Value>,
}

impl FocusSpec {
    pub fn from_json(text: &str) -> Result<Self, FocusDevError> {
        let spec: FocusSpec =
            serde_json::from_str(text).map_err(|e| FocusDevError::Spec(e.to_string()))?;
        if spec.format_version != SPEC_FORMAT_VERSION {
            return Err(FocusDevError::Spec(format!(
                "format_version {} is not {SPEC_FORMAT_VERSION}",
                spec.format_version
            )));
        }
        Ok(spec)
    }

    /// The rows with every preset use expanded, in order.
    pub fn expanded_rows(&self, presets: &PresetFile) -> Result<Vec<SpecRow>, FocusDevError> {
        let mut out = Vec::new();
        for entry in &self.rows {
            match entry {
                SpecEntry::Row(r) => out.push((**r).clone()),
                SpecEntry::Preset(u) => out.extend(preset::expand(presets, u)?),
            }
        }
        Ok(out)
    }

    /// Expands presets and converts to the one writer's row shape. Conversion errors
    /// (a guard value of the wrong JSON type, a config that is not an object) are
    /// reported here; every other rule is shape::check's.
    pub fn to_seed(&self, presets: &PresetFile) -> Result<FocusSeed, FocusDevError> {
        let f = &self.focus;
        let mut compositions = Vec::new();
        for r in self.expanded_rows(presets)? {
            compositions.push(row_to_seed(&r)?);
        }
        let mut questions = Vec::new();
        for q in &self.launch {
            let mut options = Vec::new();
            for o in &q.options {
                let envelope = match &o.envelope {
                    None => None,
                    Some(v @ Value::Object(_)) => Some(v.to_string()),
                    Some(_) => {
                        return Err(FocusDevError::Spec(format!(
                            "question '{}' option '{}': envelope must be a JSON object",
                            q.question_id, o.option_id
                        )))
                    }
                };
                options.push(LaunchOptionSeed {
                    option_id: o.option_id.clone(),
                    label: o.label.clone(),
                    hint: o.hint.clone(),
                    envelope,
                });
            }
            questions.push(LaunchQuestionSeed {
                question_id: q.question_id.clone(),
                slot: q.slot.clone(),
                kind: q.kind.clone(),
                asked: q.asked,
                default_option: q.default_option.clone(),
                prompt: q.prompt.clone(),
                options,
            });
        }
        Ok(FocusSeed {
            focus: FocusSeedRow {
                focus_id: f.focus_id.clone(),
                display_name: f.display_name.clone(),
                description: f.description.clone(),
                version: f.version.clone(),
                max_routing_tier: f.max_routing_tier.clone(),
                output_type: f.output_type.clone(),
                suggest_in_focuses: f.suggest_in_focuses.clone(),
                multi_source_validation: f.multi_source_validation,
                generic_title_template: f.generic_title_template.clone(),
                high_priority_trigger_anchor_field: f
                    .high_priority_trigger
                    .as_ref()
                    .map(|t| t.anchor_field.clone()),
                high_priority_trigger_offset: f
                    .high_priority_trigger
                    .as_ref()
                    .map(|t| t.offset.clone()),
                status: f.status.clone(),
                collection_posture: f.collection_posture.clone(),
            },
            compositions,
            questions,
            ui_one_liner: self.ui_one_liner.clone(),
        })
    }
}

/// A guard as the three columns (var, op, value text), the encoding the loader reads.
fn guard_columns(
    row: &str,
    g: &SpecGuard,
) -> Result<(String, String, Option<String>), FocusDevError> {
    let value = match (g.op.as_str(), &g.value) {
        ("eq" | "ne", Some(Value::String(s))) => Some(s.clone()),
        ("in", Some(v @ Value::Array(_))) => Some(v.to_string()),
        ("present" | "absent", None) => None,
        (op @ ("eq" | "ne"), _) => {
            return Err(FocusDevError::Spec(format!(
                "row '{row}': the value of {op} is one JSON string"
            )))
        }
        ("in", _) => {
            return Err(FocusDevError::Spec(format!(
                "row '{row}': the value of in is a JSON array of strings"
            )))
        }
        ("present" | "absent", Some(_)) => {
            return Err(FocusDevError::Spec(format!(
                "row '{row}': {} takes no value",
                g.op
            )))
        }
        (other, _) => {
            return Err(FocusDevError::Spec(format!(
                "row '{row}': unknown guard operator '{other}'"
            )))
        }
    };
    Ok((g.var.clone(), g.op.clone(), value))
}

fn row_to_seed(r: &SpecRow) -> Result<CompositionSeed, FocusDevError> {
    let id = r.action_id.as_str();
    if !r.config.is_object() {
        return Err(FocusDevError::Spec(format!(
            "row '{id}': config must be a JSON object"
        )));
    }
    let mut c = CompositionSeed::plain(id, r.block.as_deref(), &r.config.to_string());
    if let Some(g) = &r.guard {
        let (v, o, val) = guard_columns(id, g)?;
        c.guard_var = Some(v);
        c.guard_op = Some(o);
        c.guard_value = val;
    }
    if !r.guard_clauses.is_empty() {
        // Each clause is validated by guard::parse_clauses in shape::check, so a wrong
        // value type is reported there; the column is the JSON array of the spec's own clauses.
        c.guard_clauses = Some(
            serde_json::to_string(&r.guard_clauses)
                .map_err(|e| FocusDevError::Spec(format!("row '{id}': guard_clauses: {e}")))?,
        );
    }
    if let Some(t) = &r.schedule_trigger {
        c.schedule_trigger_anchor_field = Some(t.anchor_field.clone());
        c.schedule_trigger_offset = Some(t.offset.clone());
    }
    if let Some(s) = &r.store {
        c.store_op = Some(s.op.clone());
        c.store_target = s.target.clone();
        c.store_args = match &s.args {
            None => None,
            Some(v @ Value::Object(_)) => Some(v.to_string()),
            Some(_) => {
                return Err(FocusDevError::Spec(format!(
                    "row '{id}': store args must be a JSON object of slot to variable name"
                )))
            }
        };
    }
    Ok(c)
}
