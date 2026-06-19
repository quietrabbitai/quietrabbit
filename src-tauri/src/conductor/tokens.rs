// src-tauri/src/conductor/tokens.rs
//
// System token constants and StepDefinition.
// Ported from conductor/tokens.py.
//
// SYSTEM_TOKENS: reserved variable names injected by the Conductor into
// prompt templates. output_var names in .focus files must not collide.
//
// StepDefinition: internal representation of one step from a .focus file.
// Populated during Phase 1 LOAD. Immutable by construction (no &mut methods).
// Python used frozen=True + MappingProxyType — Rust ownership gives this
// for free once the struct is constructed.
//
// validate_step(): called by the Conductor during Phase 1 LOAD.
// Returns Vec<String> of error messages. Empty = valid.
//
// Rename history (CLAUDE.md):
//   path_context  → focus_context   (D6-224/D6-225)
//   space_context → life_context    (interim)
//   life_context  → persona_context (D6-298/D6-323)

use std::collections::HashMap;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// SYSTEM_TOKENS
// ---------------------------------------------------------------------------

/// Reserved variable names injected by the Conductor into prompt templates.
/// output_var names declared in .focus files must not appear in this list.
/// Python oracle: frozenset in conductor/tokens.py — 5 tokens, verified.
pub const SYSTEM_TOKENS: [&str; 5] = [
    "user_input",       // the user's current request
    "persona_context",  // persona-level shared context
    "voice_profile",    // assembled voice profile for this step
    "previous_output",  // output_var from the immediately preceding step
    "focus_context",    // focus-level metadata (name, description)
];

/// O(1)-equivalent membership test for a 5-element static array.
/// Equivalent to Python's `token in SYSTEM_TOKENS`.
pub fn is_system_token(name: &str) -> bool {
    SYSTEM_TOKENS.contains(&name)
}

// ---------------------------------------------------------------------------
// StepType
// ---------------------------------------------------------------------------

/// Valid step_type values for a StepDefinition.
/// Python oracle: Literal["generate", "voice_transform", "post_process"]
///
/// Using an enum means invalid step_type values are impossible to construct —
/// the Python validate_step() string check is eliminated at the type level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepType {
    Generate,
    VoiceTransform,
    PostProcess,
}

impl Default for StepType {
    fn default() -> Self {
        Self::Generate
    }
}

impl FromStr for StepType {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "generate"        => Ok(Self::Generate),
            "voice_transform" => Ok(Self::VoiceTransform),
            "post_process"    => Ok(Self::PostProcess),
            other => Err(format!(
                "unknown step_type '{}'. Must be: generate | voice_transform | post_process",
                other
            )),
        }
    }
}

impl StepType {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Generate       => "generate",
            Self::VoiceTransform => "voice_transform",
            Self::PostProcess    => "post_process",
        }
    }
}

// ---------------------------------------------------------------------------
// FieldRequirement
// ---------------------------------------------------------------------------

/// Valid values for field_requirements map entries.
/// Python oracle: Literal["recommended", "optional", "not_needed"]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FieldRequirement {
    Recommended,
    Optional,
    NotNeeded,
}

impl FromStr for FieldRequirement {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "recommended" => Ok(Self::Recommended),
            "optional"    => Ok(Self::Optional),
            "not_needed"  => Ok(Self::NotNeeded),
            other => Err(format!(
                "unknown field_requirement '{}'. Must be: recommended | optional | not_needed",
                other
            )),
        }
    }
}

// ---------------------------------------------------------------------------
// StepDefinition
// ---------------------------------------------------------------------------

/// Internal representation of a single step from a .focus file.
/// Populated during Phase 1 LOAD. Immutable by construction —
/// no &mut methods after build. Python oracle: frozen dataclass.
///
/// dict fields (field_requirements, options_override) are owned values;
/// Rust ownership enforces immutability once the struct is built.
///
/// routing_tier: stored as u8 with runtime validation in validate_step().
/// TODO: consider RoutingTier enum (Tier1/Tier2/Tier3) to make invalid
/// values impossible at the type level — deferred pending full conductor port.
///
/// options_override: HashMap<String, serde_json::Value> mirrors Python's
/// dict[str, object]. Schema is intentionally open-ended at this layer;
/// typed options struct deferred to focus loader implementation.
#[derive(Debug, Clone)]
pub struct StepDefinition {
    pub step_id: String,
    pub display_name: String,
    pub guide_id: String,
    pub task_type: String,     // validated against task_types.yaml at LOAD
    pub routing_tier: u8,      // 1, 2, or 3 — see validate_step()
    pub step_type: StepType,
    pub output_var: Option<String>,
    pub prompt_template: String,
    pub field_requirements: HashMap<String, FieldRequirement>,
    pub options_override: HashMap<String, serde_json::Value>,
}

// ---------------------------------------------------------------------------
// validate_step
// ---------------------------------------------------------------------------

/// Validate a StepDefinition after Phase 1 LOAD.
/// Returns Vec of error strings. Empty vec = valid.
/// Python oracle: validate_step() in conductor/tokens.py.
///
/// Checks:
///   1. output_var does not collide with a SYSTEM_TOKEN
///   2. routing_tier is 1, 2, or 3
///
/// step_type validation is omitted: StepType enum makes invalid values
/// impossible to construct (replaces Python string check).
pub fn validate_step(step: &StepDefinition) -> Vec<String> {
    let mut errors = Vec::new();

    if let Some(ref var) = step.output_var {
        if is_system_token(var) {
            errors.push(format!(
                "Step '{}': output_var '{}' collides with a system token. \
                 System tokens: {:?}",
                step.step_id, var, SYSTEM_TOKENS,
            ));
        }
    }

    if step.routing_tier == 0 || step.routing_tier > 3 {
        errors.push(format!(
            "Step '{}': routing_tier must be 1, 2, or 3. Got: {}",
            step.step_id, step.routing_tier
        ));
    }

    errors
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal_step(output_var: Option<&str>, routing_tier: u8) -> StepDefinition {
        StepDefinition {
            step_id: "test-step".to_owned(),
            display_name: "Test Step".to_owned(),
            guide_id: "writing-voice".to_owned(),
            task_type: "generate_text".to_owned(),
            routing_tier,
            step_type: StepType::Generate,
            output_var: output_var.map(|s| s.to_owned()),
            prompt_template: "Hello {user_input}".to_owned(),
            field_requirements: HashMap::new(),
            options_override: HashMap::new(),
        }
    }

    #[test]
    fn valid_step_no_errors() {
        let step = minimal_step(Some("draft_output"), 1);
        assert!(validate_step(&step).is_empty());
    }

    #[test]
    fn valid_step_no_output_var() {
        let step = minimal_step(None, 2);
        assert!(validate_step(&step).is_empty());
    }

    #[test]
    fn output_var_collides_with_system_token() {
        let step = minimal_step(Some("user_input"), 2);
        let errs = validate_step(&step);
        assert_eq!(errs.len(), 1);
        assert!(errs[0].contains("collides with a system token"));
    }

    #[test]
    fn multiple_validation_failures() {
        // output_var collides + routing_tier out of range → 2 errors
        let step = minimal_step(Some("user_input"), 7);
        let errs = validate_step(&step);
        assert_eq!(errs.len(), 2);
    }

    #[test]
    fn routing_tier_zero_is_invalid() {
        let step = minimal_step(None, 0);
        let errs = validate_step(&step);
        assert_eq!(errs.len(), 1);
        assert!(errs[0].contains("routing_tier must be 1, 2, or 3"));
    }

    #[test]
    fn routing_tier_4_is_invalid() {
        let step = minimal_step(None, 4);
        let errs = validate_step(&step);
        assert_eq!(errs.len(), 1);
    }

    #[test]
    fn routing_tier_3_is_valid() {
        let step = minimal_step(Some("result"), 3);
        assert!(validate_step(&step).is_empty());
    }

    #[test]
    fn is_system_token_membership() {
        assert!(is_system_token("user_input"));
        assert!(is_system_token("persona_context"));
        assert!(is_system_token("voice_profile"));
        assert!(is_system_token("previous_output"));
        assert!(is_system_token("focus_context"));
        assert!(!is_system_token("draft_output"));
        assert!(!is_system_token(""));
    }

    #[test]
    fn step_type_from_str_valid() {
        assert_eq!("generate".parse::<StepType>().unwrap(), StepType::Generate);
        assert_eq!("voice_transform".parse::<StepType>().unwrap(), StepType::VoiceTransform);
        assert_eq!("post_process".parse::<StepType>().unwrap(), StepType::PostProcess);
    }

    #[test]
    fn step_type_from_str_invalid() {
        assert!("unknown".parse::<StepType>().is_err());
        assert!("Generate".parse::<StepType>().is_err()); // case-sensitive
    }

    #[test]
    fn step_type_as_str_roundtrip() {
        assert_eq!(StepType::Generate.as_str(), "generate");
        assert_eq!(StepType::VoiceTransform.as_str(), "voice_transform");
        assert_eq!(StepType::PostProcess.as_str(), "post_process");
    }

    #[test]
    fn field_requirement_from_str() {
        assert_eq!("recommended".parse::<FieldRequirement>().unwrap(), FieldRequirement::Recommended);
        assert_eq!("optional".parse::<FieldRequirement>().unwrap(), FieldRequirement::Optional);
        assert_eq!("not_needed".parse::<FieldRequirement>().unwrap(), FieldRequirement::NotNeeded);
        assert!("invalid".parse::<FieldRequirement>().is_err());
    }
}
