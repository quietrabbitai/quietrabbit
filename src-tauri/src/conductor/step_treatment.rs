// src-tauri/src/conductor/step_treatment.rs
//
// items.id=921 (decisions.id=942 addendum 5 (e), decisions.id=943 as amended):
// the privacy treatment of ONE step attempt, derived from the DESTINATION the
// step actually goes to rather than from the ceiling it is allowed to reach.
//
// WHY. For a model-block step the resolver (model_resolver.rs, items.id=896)
// picks the model BEFORE Gate 1. The ceiling (StepContext.effective_access, and
// the execution_tier, raw_abstraction and abstraction_tier lifecycle computed
// from it, floor clamp included) only says what is ALLOWED. A local pick under an
// external ceiling used to be projected, abstracted, floor-consent gated, scored
// and recorded as if it had gone external. This module is the one place that
// turns (ceiling values, requirement, pick) into the values Gate 1, the floor
// consent gate, the render, the voice scan, Gate 2, the sensitivity score, the
// recorded tier, Gate 3 and the network branch all read.
//
// THE RULE (pure, no I/O, no async):
//   * Basis::Ordinary (a step with no requirement, or a bound API Chat run): the
//     ceiling values pass through UNCHANGED. `hosted` is the ceiling test every
//     ordinary step has always used (access != LocalOnly).
//   * Basis::Resolved + local pick: LOCAL treatment. LocalOnly, tier 1, raw and
//     abstraction 1, nothing is floor-clamped because nothing leaves the device.
//   * Basis::Resolved + hosted pick: EXTERNAL treatment at the REACH,
//     min(ceiling, the requirement's highest tier). Never looser than the ceiling
//     values it was given: the tiers are the max of the derived and given ones.
//   * A hosted pick under a local-only ceiling, a hosted pick whose reach is
//     local-only, and a selection that is half local and half hosted are
//     DISAGREEMENTS and are refused (the strict direction), never skipped.
//
// The value is a LOCAL of one execute_once attempt. It is never stored on
// StepContext or FocusRun, so a retry or a resumed run derives it again from its
// own selection (a scan test in executor.rs pins that).
//
// This module names no provider and no model: it sees only whether a selection is
// local or hosted and the typed tiers (a scan test below pins that).

use crate::conductor::lifecycle::execution_tier_ordinal;
use crate::conductor::tokens::ExternalAccess;

/// What the selection is, as far as privacy goes. Built from the two fields of a
/// selected model that must agree (see Chosen::from_parts).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Chosen {
    Local,
    Hosted,
}

impl Chosen {
    /// A selection is wholly local (no provider, no hosted row) or wholly hosted
    /// (both). Anything else is refused: SelectedModel's constructors cannot build
    /// it, and this keeps that true if a field is ever set by hand.
    pub fn from_parts(has_provider: bool, has_hosted_row: bool) -> Result<Chosen, TreatmentError> {
        match (has_provider, has_hosted_row) {
            (false, false) => Ok(Chosen::Local),
            (true, true) => Ok(Chosen::Hosted),
            _ => Err(TreatmentError::InconsistentSelection),
        }
    }
}

/// The ceiling-keyed values lifecycle computed (the StepContext fields).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CeilingValues {
    pub access: ExternalAccess,
    pub execution_tier: u8,
    pub abstraction_tier: u8,
    pub raw_abstraction: u8,
}

/// How the step's destination is decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Basis {
    /// No resolver: the ceiling values are the treatment.
    Ordinary,
    /// The resolver chose. `highest_tier` is the requirement's own highest tier.
    Resolved {
        highest_tier: ExternalAccess,
        chosen: Chosen,
    },
}

/// The treatment of one attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StepTreatment {
    /// Gate 1's fatality split, Gate 2, the voice scan and the sensitivity score
    /// key on this.
    pub access: ExternalAccess,
    pub execution_tier: u8,
    pub abstraction_tier: u8,
    pub raw_abstraction: u8,
    /// Whether the call leaves the device. The network branch, the render, the
    /// stream sink and (items.id=884) the outbound review all read this one flag.
    pub hosted: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TreatmentError {
    #[error("a hosted model was chosen under a local-only ceiling")]
    HostedUnderLocalOnly,
    #[error("a hosted model was chosen but the step's range reaches only the device")]
    HostedAboveRange,
    #[error("the selection is neither wholly local nor wholly hosted")]
    InconsistentSelection,
    #[error("{0}")]
    FloorInvariant(String),
}

impl TreatmentError {
    /// The text of the ConductorError::TierBoundaryViolation the executor raises.
    /// HostedUnderLocalOnly keeps the wording items.id=896 shipped.
    pub fn plain_language(&self, action_id: &str) -> String {
        match self {
            TreatmentError::HostedUnderLocalOnly => format!(
                "Step '{action_id}' resolved to a hosted model under a local-only ceiling. \
                 [Get help]"
            ),
            TreatmentError::HostedAboveRange => format!(
                "Step '{action_id}' resolved to a hosted model outside its allowed range. \
                 [Get help]"
            ),
            TreatmentError::InconsistentSelection => format!(
                "Step '{action_id}' resolved to a model with an inconsistent destination. \
                 [Get help]"
            ),
            TreatmentError::FloorInvariant(text) => text.clone(),
        }
    }
}

/// ADR-012 Amendment 3 / D6-348 floor invariants over a set of tier values. The
/// text is the one execute_once has always raised. None = the values hold.
pub fn floor_invariant_violation(
    execution_tier: u8,
    abstraction_tier: u8,
    raw_abstraction: u8,
) -> Option<String> {
    if execution_tier > 1 {
        if abstraction_tier < 2 {
            return Some(format!(
                "Floor invariant: execution_tier={execution_tier} requires \
                 abstraction_tier >= 2, got {abstraction_tier}. \
                 Lifecycle configuration error. [Get help]"
            ));
        }
        if raw_abstraction > abstraction_tier {
            return Some(format!(
                "Floor invariant: raw_abstraction ({raw_abstraction}) \
                 > abstraction_tier ({abstraction_tier}). \
                 Lifecycle configuration error. [Get help]"
            ));
        }
    } else if abstraction_tier != raw_abstraction {
        return Some(format!(
            "Tier 1 invariant: abstraction_tier ({abstraction_tier}) \
             != raw_abstraction ({raw_abstraction}) at execution_tier=1. \
             Lifecycle configuration error. [Get help]"
        ));
    }
    None
}

/// Derive the treatment. See the module header for the rule.
pub fn derive(ceiling: CeilingValues, basis: Basis) -> Result<StepTreatment, TreatmentError> {
    let treatment = match basis {
        Basis::Ordinary => StepTreatment {
            access: ceiling.access,
            execution_tier: ceiling.execution_tier,
            abstraction_tier: ceiling.abstraction_tier,
            raw_abstraction: ceiling.raw_abstraction,
            hosted: ceiling.access != ExternalAccess::LocalOnly,
        },
        Basis::Resolved {
            chosen: Chosen::Local,
            ..
        } => StepTreatment {
            access: ExternalAccess::LocalOnly,
            execution_tier: 1,
            abstraction_tier: 1,
            raw_abstraction: 1,
            hosted: false,
        },
        Basis::Resolved {
            highest_tier,
            chosen: Chosen::Hosted,
        } => {
            if ceiling.access == ExternalAccess::LocalOnly {
                return Err(TreatmentError::HostedUnderLocalOnly);
            }
            let reach = ceiling.access.min(highest_tier);
            if reach == ExternalAccess::LocalOnly {
                return Err(TreatmentError::HostedAboveRange);
            }
            // Never looser than the ceiling values: the max of both. The floor
            // clamp (abstraction >= 2 while external access is live) is applied
            // again here, so a ceiling value that was computed for a local step
            // cannot leave a hosted call under-abstracted.
            let execution_tier = ceiling.execution_tier.max(execution_tier_ordinal(reach));
            let abstraction_tier = ceiling.abstraction_tier.max(2);
            StepTreatment {
                access: reach,
                execution_tier,
                abstraction_tier,
                raw_abstraction: ceiling.raw_abstraction.min(abstraction_tier),
                hosted: true,
            }
        }
    };
    if let Some(text) = floor_invariant_violation(
        treatment.execution_tier,
        treatment.abstraction_tier,
        treatment.raw_abstraction,
    ) {
        return Err(TreatmentError::FloorInvariant(text));
    }
    Ok(treatment)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ACCESSES: [ExternalAccess; 4] = [
        ExternalAccess::LocalOnly,
        ExternalAccess::AnonymousRequired,
        ExternalAccess::AnonymousPreferred,
        ExternalAccess::Unrestricted,
    ];

    /// The values lifecycle::execute_step computes for a step whose ceiling is
    /// `access` under a Focus privacy tier of `privacy_tier` (1 to 3).
    fn lifecycle_values(access: ExternalAccess, privacy_tier: u8) -> CeilingValues {
        let execution_tier = execution_tier_ordinal(access);
        let raw = privacy_tier.min(execution_tier);
        let abstraction = if access != ExternalAccess::LocalOnly {
            raw.max(2)
        } else {
            raw
        };
        CeilingValues {
            access,
            execution_tier,
            abstraction_tier: abstraction,
            raw_abstraction: raw,
        }
    }

    fn every_lifecycle_value() -> Vec<CeilingValues> {
        let mut v = Vec::new();
        for a in ACCESSES {
            for p in 1..=3u8 {
                v.push(lifecycle_values(a, p));
            }
        }
        v
    }

    #[test]
    fn an_ordinary_step_passes_every_ceiling_value_through_unchanged() {
        for c in every_lifecycle_value() {
            let t = derive(c, Basis::Ordinary).unwrap();
            assert_eq!(t.access, c.access);
            assert_eq!(t.execution_tier, c.execution_tier);
            assert_eq!(t.abstraction_tier, c.abstraction_tier);
            assert_eq!(t.raw_abstraction, c.raw_abstraction);
            assert_eq!(t.hosted, c.access != ExternalAccess::LocalOnly);
        }
    }

    #[test]
    fn a_local_pick_gets_the_local_treatment_under_every_ceiling() {
        for c in every_lifecycle_value() {
            for highest in ACCESSES {
                let t = derive(
                    c,
                    Basis::Resolved {
                        highest_tier: highest,
                        chosen: Chosen::Local,
                    },
                )
                .unwrap();
                assert_eq!(t.access, ExternalAccess::LocalOnly);
                assert_eq!(
                    (t.execution_tier, t.abstraction_tier, t.raw_abstraction),
                    (1, 1, 1),
                    "nothing is clamped when nothing leaves the device ({c:?})"
                );
                assert!(!t.hosted);
            }
        }
    }

    #[test]
    fn a_hosted_pick_is_external_at_the_reach_and_never_looser_than_the_ceiling_values() {
        let mut ok = 0;
        for c in every_lifecycle_value() {
            for highest in ACCESSES {
                let basis = Basis::Resolved {
                    highest_tier: highest,
                    chosen: Chosen::Hosted,
                };
                let reach = c.access.min(highest);
                match derive(c, basis) {
                    Ok(t) => {
                        ok += 1;
                        assert!(c.access != ExternalAccess::LocalOnly);
                        assert!(reach != ExternalAccess::LocalOnly);
                        assert_eq!(t.access, reach);
                        assert!(t.hosted);
                        assert!(t.execution_tier >= c.execution_tier);
                        assert!(t.abstraction_tier >= c.abstraction_tier);
                        assert!(t.abstraction_tier >= 2);
                        assert!(t.raw_abstraction <= c.raw_abstraction);
                        assert_eq!(
                            floor_invariant_violation(
                                t.execution_tier,
                                t.abstraction_tier,
                                t.raw_abstraction
                            ),
                            None
                        );
                    }
                    Err(e) => {
                        assert!(
                            c.access == ExternalAccess::LocalOnly
                                || reach == ExternalAccess::LocalOnly,
                            "only a disagreement is refused: {e}"
                        );
                    }
                }
            }
        }
        assert!(ok > 0);
    }

    #[test]
    fn a_hosted_pick_under_a_local_only_ceiling_is_refused_with_the_shipped_wording() {
        let c = lifecycle_values(ExternalAccess::LocalOnly, 1);
        let e = derive(
            c,
            Basis::Resolved {
                highest_tier: ExternalAccess::Unrestricted,
                chosen: Chosen::Hosted,
            },
        )
        .unwrap_err();
        assert_eq!(e, TreatmentError::HostedUnderLocalOnly);
        assert_eq!(
            e.plain_language("draft"),
            "Step 'draft' resolved to a hosted model under a local-only ceiling. [Get help]"
        );
    }

    #[test]
    fn a_hosted_pick_from_a_range_that_reaches_only_the_device_is_refused() {
        let c = lifecycle_values(ExternalAccess::Unrestricted, 3);
        let e = derive(
            c,
            Basis::Resolved {
                highest_tier: ExternalAccess::LocalOnly,
                chosen: Chosen::Hosted,
            },
        )
        .unwrap_err();
        assert_eq!(e, TreatmentError::HostedAboveRange);
    }

    #[test]
    fn the_reach_is_the_lower_of_the_ceiling_and_the_requirement_tier() {
        let c = lifecycle_values(ExternalAccess::Unrestricted, 3);
        let t = derive(
            c,
            Basis::Resolved {
                highest_tier: ExternalAccess::AnonymousRequired,
                chosen: Chosen::Hosted,
            },
        )
        .unwrap();
        assert_eq!(t.access, ExternalAccess::AnonymousRequired);
        // The ceiling values already at tier 3 are kept: never looser.
        assert_eq!(t.execution_tier, 3);
        assert_eq!(t.abstraction_tier, 3);
        let low = lifecycle_values(ExternalAccess::AnonymousRequired, 1);
        let t = derive(
            low,
            Basis::Resolved {
                highest_tier: ExternalAccess::Unrestricted,
                chosen: Chosen::Hosted,
            },
        )
        .unwrap();
        assert_eq!(t.access, ExternalAccess::AnonymousRequired);
        assert_eq!((t.execution_tier, t.abstraction_tier), (2, 2));
        assert_eq!(t.raw_abstraction, 1, "the floor clamp stays visible");
    }

    #[test]
    fn ceiling_values_computed_for_a_local_step_cannot_under_abstract_a_hosted_call() {
        // An inconsistent ceiling (tier 1 values under an external access) is
        // raised, not trusted.
        let c = CeilingValues {
            access: ExternalAccess::AnonymousPreferred,
            execution_tier: 1,
            abstraction_tier: 1,
            raw_abstraction: 1,
        };
        let t = derive(
            c,
            Basis::Resolved {
                highest_tier: ExternalAccess::AnonymousPreferred,
                chosen: Chosen::Hosted,
            },
        )
        .unwrap();
        assert_eq!(t.execution_tier, 3);
        assert_eq!(t.abstraction_tier, 2);
        assert!(t.hosted);
    }

    #[test]
    fn a_selection_that_is_half_local_and_half_hosted_is_refused() {
        assert_eq!(Chosen::from_parts(false, false), Ok(Chosen::Local));
        assert_eq!(Chosen::from_parts(true, true), Ok(Chosen::Hosted));
        assert_eq!(
            Chosen::from_parts(true, false),
            Err(TreatmentError::InconsistentSelection)
        );
        assert_eq!(
            Chosen::from_parts(false, true),
            Err(TreatmentError::InconsistentSelection)
        );
    }

    #[test]
    fn a_ceiling_that_breaks_a_floor_invariant_is_refused_for_an_ordinary_step_too() {
        let bad = CeilingValues {
            access: ExternalAccess::AnonymousRequired,
            execution_tier: 2,
            abstraction_tier: 1,
            raw_abstraction: 1,
        };
        assert!(matches!(
            derive(bad, Basis::Ordinary),
            Err(TreatmentError::FloorInvariant(_))
        ));
    }

    #[test]
    fn the_floor_invariant_texts_are_the_ones_execute_once_raised() {
        assert_eq!(
            floor_invariant_violation(2, 1, 1).unwrap(),
            "Floor invariant: execution_tier=2 requires abstraction_tier >= 2, got 1. \
             Lifecycle configuration error. [Get help]"
        );
        assert_eq!(
            floor_invariant_violation(2, 2, 3).unwrap(),
            "Floor invariant: raw_abstraction (3) > abstraction_tier (2). \
             Lifecycle configuration error. [Get help]"
        );
        assert_eq!(
            floor_invariant_violation(1, 2, 1).unwrap(),
            "Tier 1 invariant: abstraction_tier (2) != raw_abstraction (1) at \
             execution_tier=1. Lifecycle configuration error. [Get help]"
        );
        assert_eq!(floor_invariant_violation(1, 1, 1), None);
        assert_eq!(floor_invariant_violation(3, 3, 2), None);
    }

    /// The module sees whether a selection is local or hosted and the typed
    /// tiers, never which provider or model.
    #[test]
    fn the_module_names_no_provider_and_no_model() {
        let src = include_str!("step_treatment.rs");
        let code = src.split("#[cfg(test)]\nmod tests {").next().unwrap();
        let lower = code.to_lowercase();
        for word in [
            "llama",
            "qwen",
            "gemma",
            "deepseek",
            "mistral",
            "openai",
            "anthropic",
            "gpt-",
            "duck",
            "provider_id",
            "model_id",
        ] {
            assert!(!lower.contains(word), "step_treatment.rs names '{word}'");
        }
    }
}
