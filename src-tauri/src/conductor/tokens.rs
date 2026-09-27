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

use super::lifecycle::HighPriorityTrigger;

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

    /// items.id=574 (reenter_step()): whether a step of this type is safe to
    /// re-run with new information, given only what the type itself does --
    /// never a per-instance customization choice (see StepDefinition::revisitable's
    /// own doc comment for why there is no .focus YAML override). executor.rs
    /// does not yet branch on step_type at all, so this is a product judgment
    /// call, not something derivable from current execution behavior --
    /// confirmed against every shipped .focus file (Jason, 2026-09-26):
    /// generate appears 10 times across 4 Focuses, voice_transform exactly
    /// once (writing-assistant.focus's "Refining your voice..." step, a real
    /// style-refinement step, clearly safe to redo), post_process zero times
    /// anywhere -- so PostProcess defaults false (no evidence it is safe; the
    /// name itself suggests finalization/export/delivery side effects).
    pub fn default_revisitable(&self) -> bool {
        matches!(self, Self::Generate | Self::VoiceTransform)
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
/// external_access_override (below) is derived from this same value at
/// parse time (lifecycle.rs's parse_focus_definition()) — no separate
/// .focus YAML field for it; the YAML field itself is still the raw numeric
/// 1/2/3 today, converted to ExternalAccess at parse time (AnonymousPreferred
/// has no YAML-authorable form yet). requires_user_handoff (below) is a
/// genuinely separate .focus YAML field as of items.id=528 Phase 2 — see its
/// own doc comment; the two are independently derived, not coupled.
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
    /// routing_tier ALONE at parse time: never Some when routing_tier is
    /// Unrestricted (corrected by items.id=528 Phase 2 — previously gated on
    /// requires_user_handoff instead, which silently broke the moment the
    /// two could diverge; see lifecycle.rs::parse_focus_definition()'s own
    /// derivation comment for the full reasoning). Do not gate this field on
    /// requires_user_handoff — the two are independently derived.
    pub external_access_override: Option<ExternalAccess>,
    /// items.id=574 (reenter_step()): whether a user may jump back to this
    /// step with new information and re-run forward from it. Derived
    /// PURELY from step_type at parse time (StepType::default_revisitable())
    /// -- unlike requires_user_handoff, there is deliberately no .focus YAML
    /// field to author this per-instance. requires_user_handoff's
    /// authored-with-derived-fallback pattern exists specifically for
    /// back-compat with .focus files written before that field existed;
    /// revisitable has no legacy files to be compatible with, so whether a
    /// KIND of step is safe to re-run stays a property of what the step
    /// type does, not a per-step customization choice (Jason, 2026-09-26).
    pub revisitable: bool,
    /// items.id=439 (Part 6d): the "pause and hand off to the user" signal,
    /// fully decoupled from external_access — checked directly in
    /// lifecycle.rs's EXECUTE step loop. As of items.id=528 Phase 2, authored
    /// directly via its own .focus YAML field (`requires_user_handoff`) when
    /// present; falls back to the legacy `routing_tier == 3` rule when the
    /// YAML field is absent, for back-compat with every Focus authored
    /// before this field existed.
    pub requires_user_handoff: bool,
    /// items.id=496: composition-authored steps only (None for every
    /// YAML-authored step, always). Some(id) means this step's actual
    /// execution is delegated to a registered block handler keyed by `id`
    /// instead of today's StepExecutor/prompt-template pipeline --
    /// execute_step() dispatches on this field. No handler is registered
    /// for any id yet; a composition row citing one is rejected at LOAD
    /// (conductor::lifecycle::load_focus_definition_from_db()), never at
    /// execute_step() itself. See lifecycle.rs's execute_step() doc comment
    /// for the dispatch shape.
    pub block_stable_id: Option<String>,
    /// items.id=496: the raw customization payload for a composition row
    /// whose block_stable_id is Some -- opaque at this layer, same
    /// "schema intentionally open-ended, typed struct deferred" precedent
    /// options_override already established. Always None when
    /// block_stable_id is None (the payload becomes this struct's own
    /// fields instead, via the same derivation path a YAML step uses --
    /// see load_focus_definition_from_db()).
    pub block_customization: Option<serde_json::Value>,
    /// items.id=496 (Q2, per-composition-row placement): a one-shot,
    /// date-anchored gate on this step's execution eligibility, reusing
    /// decisions.id=712's HighPriorityTrigger vocabulary verbatim (same
    /// anchor_field/offset/is_active() shape, same parse_offset()).
    /// Composition-authored steps only -- always None for a YAML-authored
    /// step. Checked by execute()'s loop immediately before the
    /// requires_user_handoff check; see that loop's own comment for the
    /// gate shape and conductor::scheduled_sweep for how a fired trigger
    /// resumes the run (via resume_run/rehydrate_focus_run, not a bypass).
    pub schedule_trigger: Option<HighPriorityTrigger>,
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

    fn minimal_step(
        output_var: Option<&str>,
        routing_tier: u8,
        requires_user_handoff: bool,
    ) -> StepDefinition {
        // Mirrors lifecycle.rs's parse_focus_definition() derivation exactly
        // (items.id=528 Phase 2): external_access_override depends on
        // routing_tier ALONE (None only for Unrestricted) -- independent of
        // requires_user_handoff, which callers now pass directly instead of
        // this helper re-deriving it from routing_tier==3 (the second,
        // uncoupled copy of that derivation items.id=529 already flagged and
        // this item finishes decoupling).
        let access = match routing_tier {
            1 => ExternalAccess::LocalOnly,
            2 => ExternalAccess::AnonymousRequired,
            3 => ExternalAccess::Unrestricted,
            other => panic!("minimal_step(): routing_tier must be 1, 2, or 3, got {other}"),
        };
        let external_access_override = if access == ExternalAccess::Unrestricted {
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
            revisitable: StepType::Generate.default_revisitable(),
            block_stable_id: None,
            block_customization: None,
            schedule_trigger: None,
        }
    }

    #[test]
    fn valid_step_no_errors() {
        let step = minimal_step(Some("draft_output"), 1, false);
        assert!(validate_step(&step).is_empty());
    }

    #[test]
    fn valid_step_no_output_var() {
        let step = minimal_step(None, 2, false);
        assert!(validate_step(&step).is_empty());
    }

    #[test]
    fn output_var_collides_with_system_token() {
        let step = minimal_step(Some("user_input"), 2, false);
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
        let step = minimal_step(Some("result"), 3, true);
        assert!(step.requires_user_handoff);
        assert_eq!(step.external_access_override, None);
    }

    #[test]
    fn minimal_step_routing_tier_1_has_override_not_handoff() {
        let step = minimal_step(None, 1, false);
        assert!(!step.requires_user_handoff);
        assert_eq!(
            step.external_access_override,
            Some(ExternalAccess::LocalOnly)
        );
    }

    // -- requires_user_handoff / external_access_override decoupling
    //    (items.id=528 Phase 2) --------------------------------------------
    //
    // Before this fix, external_access_override was gated on
    // requires_user_handoff instead of routing_tier, which happened to be
    // invisible because the two conditions always coincided (routing_tier==3
    // was the only way to get requires_user_handoff==true). These tests
    // exercise exactly the case that coupling would have gotten wrong: a
    // step whose routing_tier is NOT Unrestricted but which still declares
    // requires_user_handoff: true via its own .focus YAML field.

    #[test]
    fn requires_user_handoff_true_at_anonymous_required_keeps_override() {
        let step = minimal_step(None, 2, true);
        assert!(step.requires_user_handoff);
        assert_eq!(
            step.external_access_override,
            Some(ExternalAccess::AnonymousRequired)
        );
    }

    #[test]
    fn requires_user_handoff_true_at_local_only_keeps_override() {
        let step = minimal_step(None, 1, true);
        assert!(step.requires_user_handoff);
        assert_eq!(
            step.external_access_override,
            Some(ExternalAccess::LocalOnly)
        );
    }

    // -- StepType::default_revisitable() (items.id=574 follow-up) ----------
    //
    // Confirmed against every shipped .focus file (Jason, 2026-09-26):
    // generate x10, voice_transform x1 (a real style-refinement step),
    // post_process x0 anywhere -- so only PostProcess defaults unsafe.

    #[test]
    fn generate_defaults_revisitable() {
        assert!(StepType::Generate.default_revisitable());
    }

    #[test]
    fn voice_transform_defaults_revisitable() {
        assert!(StepType::VoiceTransform.default_revisitable());
    }

    #[test]
    fn post_process_defaults_not_revisitable() {
        assert!(!StepType::PostProcess.default_revisitable());
    }
}
