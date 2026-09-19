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
    "user_input",      // the user's current request
    "persona_context", // persona-level shared context
    "voice_profile",   // assembled voice profile for this step
    "previous_output", // output_var from the immediately preceding step
    "focus_context",   // focus-level metadata (name, description)
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum StepType {
    #[default]
    Generate,
    VoiceTransform,
    PostProcess,
}

impl FromStr for StepType {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "generate" => Ok(Self::Generate),
            "voice_transform" => Ok(Self::VoiceTransform),
            "post_process" => Ok(Self::PostProcess),
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
            Self::Generate => "generate",
            Self::VoiceTransform => "voice_transform",
            Self::PostProcess => "post_process",
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
            "optional" => Ok(Self::Optional),
            "not_needed" => Ok(Self::NotNeeded),
            other => Err(format!(
                "unknown field_requirement '{}'. Must be: recommended | optional | not_needed",
                other
            )),
        }
    }
}

// ---------------------------------------------------------------------------
// ExternalAccess (items.id=439, PROVIDER_REGISTRY_AND_TIER_MODEL_SPEC.md Part 6)
// ---------------------------------------------------------------------------

/// Replaces ordinal tier comparisons at the five capability-gate call sites
/// named in Part 6e: "may this step's execution leave the device at all,"
/// decoupled from the old routing_tier==3 "pause for user handoff" signal
/// (see StepDefinition::requires_user_handoff) and from the abstraction axis
/// (focus_settings.privacy_tier, items.id=444 — untouched by this enum).
///
/// Declaration order IS the ordering derive(Ord) uses — local_only is the
/// tightest, unrestricted the loosest. anonymous_preferred means either an
/// anonymous or a full-account provider is usable, anonymous preferred when
/// there's a real choice among eligible providers (Jason, 2026-09-19) — a
/// genuine 4th level, not a hypothetical.
///
/// items.id=529: storage is this enum's own string form end to end
/// (shared_020.sql: TEXT CHECK (col IN ('local_only', 'anonymous_required',
/// 'anonymous_preferred', 'unrestricted'))) — no numeric legacy-tier
/// round-trip. Style mirrors NamedPolicy (persistence/
/// focus_provider_criteria_store.rs): as_str() plus a real FromStr impl,
/// snake_case serde matching as_str()/FromStr exactly.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, specta::Type,
)]
#[serde(rename_all = "snake_case")]
pub enum ExternalAccess {
    LocalOnly,
    AnonymousRequired,
    AnonymousPreferred,
    Unrestricted,
}

impl ExternalAccess {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::LocalOnly => "local_only",
            Self::AnonymousRequired => "anonymous_required",
            Self::AnonymousPreferred => "anonymous_preferred",
            Self::Unrestricted => "unrestricted",
        }
    }
}

impl FromStr for ExternalAccess {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "local_only" => Ok(Self::LocalOnly),
            "anonymous_required" => Ok(Self::AnonymousRequired),
            "anonymous_preferred" => Ok(Self::AnonymousPreferred),
            "unrestricted" => Ok(Self::Unrestricted),
            other => Err(format!(
                "unknown external_access '{}'. Must be: local_only | anonymous_required | \
                 anonymous_preferred | unrestricted",
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
/// routing_tier: items.id=529 retyped this from a numeric u8 to
/// ExternalAccess directly -- invalid values are unrepresentable by
/// construction (same reasoning already applied to FocusSettings.
/// max_permitted_tier, persistence/focus_settings_store.rs), so
/// validate_step() no longer bounds-checks it. Still feeds the numeric
/// execution_tier calc via ExternalAccess::min() + a bridging ordinal
/// (abstraction floor, model selection, sensitivity — lifecycle.rs/
/// executor.rs, that numeric axis itself out of scope for items.id=529).
/// external_access_override/requires_user_handoff (below) are derived from
/// this same value at parse time (lifecycle.rs's parse_focus_definition())
/// — no separate .focus YAML field for either; the YAML field itself is
/// still the raw numeric 1/2/3 today, converted to ExternalAccess at parse
/// time (AnonymousPreferred has no YAML-authorable form yet).
///
/// options_override: HashMap<String, serde_json::Value> mirrors Python's
/// dict[str, object]. Schema is intentionally open-ended at this layer;
/// typed options struct deferred to focus loader implementation.
#[derive(Debug, Clone)]
pub struct StepDefinition {
    pub step_id: String,
    pub display_name: String,
    pub guide_id: String,
    pub task_type: String, // validated against task_types.yaml at LOAD
    pub routing_tier: ExternalAccess,
    pub step_type: StepType,
    pub output_var: Option<String>,
    pub prompt_template: String,
    pub field_requirements: HashMap<String, FieldRequirement>,
    pub options_override: HashMap<String, serde_json::Value>,
    /// items.id=439 (Part 6c): tighten-only capability override relative to
    /// the Focus's own external_access ceiling. None means "inherit" — this
    /// step makes no capability claim of its own. Derived mechanically from
    /// routing_tier at parse time; never Some when requires_user_handoff is
    /// true (Part 6d: the handoff signal is fully decoupled from capability).
    pub external_access_override: Option<ExternalAccess>,
    /// items.id=439 (Part 6d): the old routing_tier==3 "pause and hand off
    /// to the user" signal, fully decoupled from external_access. Checked
    /// directly in lifecycle.rs's EXECUTE step loop.
    pub requires_user_handoff: bool,
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
///
/// step_type validation is omitted: StepType enum makes invalid values
/// impossible to construct (replaces Python string check). routing_tier
/// validation is likewise omitted as of items.id=529: ExternalAccess makes
/// an invalid value unrepresentable, so the out-of-range .focus YAML case
/// (routing_tier: 0, 4, 7, ...) is now rejected at parse time
/// (lifecycle.rs::parse_focus_definition()) rather than deferred here.
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

    errors
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal_step(output_var: Option<&str>, routing_tier: u8) -> StepDefinition {
        // Mirrors lifecycle.rs's parse_focus_definition() derivation exactly
        // (items.id=439 plan; re-confirmed identical shape by items.id=529 --
        // this was the second, uncoupled copy of routing_tier==3 flagged by
        // that item, now brought back in line with lifecycle.rs:388's
        // canonical derivation instead of also guarding against out-of-range
        // input lifecycle.rs's own code never guarded against).
        let requires_user_handoff = routing_tier == 3;
        let access = match routing_tier {
            1 => ExternalAccess::LocalOnly,
            2 => ExternalAccess::AnonymousRequired,
            3 => ExternalAccess::Unrestricted,
            other => panic!("minimal_step(): routing_tier must be 1, 2, or 3, got {other}"),
        };
        let external_access_override = if requires_user_handoff {
            None
        } else {
            Some(access)
        };
        StepDefinition {
            step_id: "test-step".to_owned(),
            display_name: "Test Step".to_owned(),
            guide_id: "writing-voice".to_owned(),
            task_type: "generate_text".to_owned(),
            routing_tier: access,
            step_type: StepType::Generate,
            output_var: output_var.map(|s| s.to_owned()),
            prompt_template: "Hello {user_input}".to_owned(),
            field_requirements: HashMap::new(),
            options_override: HashMap::new(),
            external_access_override,
            requires_user_handoff,
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
        assert_eq!(
            "voice_transform".parse::<StepType>().unwrap(),
            StepType::VoiceTransform
        );
        assert_eq!(
            "post_process".parse::<StepType>().unwrap(),
            StepType::PostProcess
        );
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
        assert_eq!(
            "recommended".parse::<FieldRequirement>().unwrap(),
            FieldRequirement::Recommended
        );
        assert_eq!(
            "optional".parse::<FieldRequirement>().unwrap(),
            FieldRequirement::Optional
        );
        assert_eq!(
            "not_needed".parse::<FieldRequirement>().unwrap(),
            FieldRequirement::NotNeeded
        );
        assert!("invalid".parse::<FieldRequirement>().is_err());
    }

    // -- ExternalAccess (items.id=439) ---------------------------------------

    #[test]
    fn external_access_ordering() {
        assert!(ExternalAccess::LocalOnly < ExternalAccess::AnonymousRequired);
        assert!(ExternalAccess::AnonymousRequired < ExternalAccess::AnonymousPreferred);
        assert!(ExternalAccess::AnonymousPreferred < ExternalAccess::Unrestricted);
    }

    #[test]
    fn external_access_from_str_round_trips_with_as_str() {
        // items.id=529: all 4 variants, AnonymousPreferred included -- the
        // whole point of retiring the legacy 1/2/3 round-trip was to give it
        // a real, round-trippable string form (it had no legacy tier slot).
        for access in [
            ExternalAccess::LocalOnly,
            ExternalAccess::AnonymousRequired,
            ExternalAccess::AnonymousPreferred,
            ExternalAccess::Unrestricted,
        ] {
            assert_eq!(access.as_str().parse::<ExternalAccess>().unwrap(), access);
        }
    }

    #[test]
    fn external_access_from_str_rejects_unknown() {
        assert!("tier_2".parse::<ExternalAccess>().is_err());
        assert!("".parse::<ExternalAccess>().is_err());
    }

    #[test]
    fn external_access_as_str() {
        assert_eq!(ExternalAccess::LocalOnly.as_str(), "local_only");
        assert_eq!(
            ExternalAccess::AnonymousRequired.as_str(),
            "anonymous_required"
        );
        assert_eq!(
            ExternalAccess::AnonymousPreferred.as_str(),
            "anonymous_preferred"
        );
        assert_eq!(ExternalAccess::Unrestricted.as_str(), "unrestricted");
    }

    #[test]
    fn minimal_step_routing_tier_3_has_handoff_not_override() {
        let step = minimal_step(Some("result"), 3);
        assert!(step.requires_user_handoff);
        assert_eq!(step.external_access_override, None);
    }

    #[test]
    fn minimal_step_routing_tier_1_has_override_not_handoff() {
        let step = minimal_step(None, 1);
        assert!(!step.requires_user_handoff);
        assert_eq!(
            step.external_access_override,
            Some(ExternalAccess::LocalOnly)
        );
    }
}
