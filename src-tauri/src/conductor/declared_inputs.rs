// src-tauri/src/conductor/declared_inputs.rs
//
// items.id=879 (decisions.id=937 points 1 and 5, decisions.id=938): the LOAD
// lint for declared inputs. Every EXTERNAL step (routing_tier not LocalOnly)
// lists the variables and tokens its prompt template may name, in its
// `declared_inputs` field (the .focus YAML step and the database customization
// JSON use the same name). The default is NONE, so an external template that
// names anything undeclared fails LOAD.
//
// This is a LOADER LINT ONLY. It changes nothing a prompt renders and there is
// no runtime filter in executor.rs; Gate 1, 2 and 3 are unchanged. A step's
// declaration is data that later items read (items.id=882 persona_context,
// 883 history, 884 outbound review).
//
// What a template name is allowed to be, for an external step:
//   * a name in the step's own `declared_inputs`, or
//   * a name in the step's own `field_requirements` (Gate 1's declaration, so it
//     is implicitly declared and is never required twice).
// Anything else is rejected, with a message of its own for each failure:
//   1. a turn-bound or session-bound name that is not declared (see
//      bound_names(): empty today, items.id=881 registers its names there and
//      the lint needs no further change);
//   2. any other undeclared name (this also wins over "nothing produces this
//      name", which is otherwise only a silent strip at render time).
// And a DECLARED name must itself be real, or LOAD fails (3): the output_var of
// an EARLIER row (a block row's output variable counts, so a step after a block
// step cannot name the block's verdict variable unless it declares it), one of
// the five system tokens, a name in the step's own field_requirements (the only
// disclosure fields a step receives: the disclosure buffer is keyed by the
// step), or a registered bound name.
//
// A LOCAL step needs no declaration. If a local step has one anyway its names
// are still checked by (3), so a typo cannot hide until a step's tier is
// raised.
//
// Names are taken from executor::find_tokens, the same extractor the renderer
// strips with, so the lint sees exactly the names the renderer would resolve.

use std::collections::HashSet;

use crate::conductor::executor::find_tokens;
use crate::conductor::tokens::{is_system_token, ActionDefinition, ExternalAccess};

/// Turn-bound and session-bound variable names (items.id=881 builds the
/// session store and registers them here). EMPTY today: no such variable
/// exists yet. An external step may name one only if it declares it.
pub fn bound_names() -> &'static [&'static str] {
    &[]
}

/// Why a Focus failed the declared-inputs lint. Each variant is its own
/// failure with its own message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeclaredInputsError {
    /// An external template names a turn-bound or session-bound variable it
    /// does not declare.
    BoundNameUndeclared { action_id: String, name: String },
    /// An external template names something it does not declare.
    Undeclared { action_id: String, name: String },
    /// A declared name is not an earlier row's output variable, a system
    /// token, a field of the step, or a registered bound name.
    UnknownDeclared { action_id: String, name: String },
}

impl std::fmt::Display for DeclaredInputsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BoundNameUndeclared { action_id, name } => write!(
                f,
                "step '{action_id}': external step names the turn-bound or session-bound \
                 variable '{{{name}}}' without declaring it in declared_inputs"
            ),
            Self::Undeclared { action_id, name } => write!(
                f,
                "step '{action_id}': external step names '{{{name}}}' but does not declare it \
                 in declared_inputs (an external step receives declared inputs only, and its \
                 field_requirements names)"
            ),
            Self::UnknownDeclared { action_id, name } => write!(
                f,
                "step '{action_id}': declared_inputs names '{name}', which is not an earlier \
                 step's output variable, a system token, a field in this step's \
                 field_requirements, or a registered bound variable"
            ),
        }
    }
}

/// Lint a Focus's steps against the registered bound names (bound_names()).
/// `row_vars[i]` is the variable row `i` records when it runs: the output_var
/// of an ordinary step, a block handler's output variable for a block row, or
/// None. The caller builds it (lifecycle::row_output_vars) because a block
/// row's variable comes from the handler registry.
pub fn lint(steps: &[ActionDefinition], row_vars: &[Option<String>]) -> Result<(), String> {
    lint_with_bound(steps, row_vars, bound_names())
}

/// lint() with an explicit bound-name registry, so tests can register a name
/// before items.id=881 exists.
pub fn lint_with_bound(
    steps: &[ActionDefinition],
    row_vars: &[Option<String>],
    bound: &[&str],
) -> Result<(), String> {
    debug_assert_eq!(steps.len(), row_vars.len());
    let mut earlier: HashSet<&str> = HashSet::new();
    for (step, var) in steps.iter().zip(row_vars.iter()) {
        // A block row has no template and no declaration; it only produces.
        if step.block_stable_id.is_none() {
            lint_step(step, &earlier, bound).map_err(|e| e.to_string())?;
        }
        if let Some(v) = var.as_deref().filter(|v| !v.is_empty()) {
            earlier.insert(v);
        }
    }
    Ok(())
}

fn lint_step(
    step: &ActionDefinition,
    earlier: &HashSet<&str>,
    bound: &[&str],
) -> Result<(), DeclaredInputsError> {
    // (3) every declared name must be real, external or not.
    for name in &step.declared_inputs {
        let known = earlier.contains(name.as_str())
            || is_system_token(name)
            || step.field_requirements.contains_key(name)
            || bound.contains(&name.as_str());
        if !known {
            return Err(DeclaredInputsError::UnknownDeclared {
                action_id: step.action_id.clone(),
                name: name.clone(),
            });
        }
    }

    if step.routing_tier == ExternalAccess::LocalOnly {
        return Ok(());
    }

    // (1) and (2): every name the template mentions is declared.
    for name in find_tokens(&step.prompt_template) {
        let declared = step.declared_inputs.iter().any(|d| d == &name)
            || step.field_requirements.contains_key(&name);
        if declared {
            continue;
        }
        return Err(if bound.contains(&name.as_str()) {
            DeclaredInputsError::BoundNameUndeclared {
                action_id: step.action_id.clone(),
                name,
            }
        } else {
            DeclaredInputsError::Undeclared {
                action_id: step.action_id.clone(),
                name,
            }
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conductor::tokens::{FieldRequirement, StepType};
    use std::collections::HashMap;

    fn step(
        id: &str,
        tier: ExternalAccess,
        template: &str,
        declared: &[&str],
        fields: &[&str],
    ) -> ActionDefinition {
        ActionDefinition {
            action_id: id.to_owned(),
            display_name: id.to_owned(),
            guide_id: "g".to_owned(),
            task_type: "general".to_owned(),
            routing_tier: tier,
            step_type: StepType::Generate,
            output_var: None,
            prompt_template: template.to_owned(),
            field_requirements: fields
                .iter()
                .map(|f| ((*f).to_owned(), FieldRequirement::Optional))
                .collect::<HashMap<_, _>>(),
            options_override: HashMap::new(),
            external_access_override: None,
            requires_user_handoff: false,
            revisitable: true,
            block_stable_id: None,
            block_customization: None,
            schedule_trigger: None,
            guard: None,
            declared_inputs: declared.iter().map(|d| (*d).to_owned()).collect(),
        }
    }

    /// Steps with `row_vars[i]` as the variable row i records.
    fn run(steps: &[(ActionDefinition, Option<&str>)], bound: &[&str]) -> Result<(), String> {
        let defs: Vec<ActionDefinition> = steps.iter().map(|(s, _)| s.clone()).collect();
        let vars: Vec<Option<String>> = steps.iter().map(|(_, v)| v.map(str::to_owned)).collect();
        lint_with_bound(&defs, &vars, bound)
    }

    const EXT: ExternalAccess = ExternalAccess::AnonymousRequired;
    const LOCAL: ExternalAccess = ExternalAccess::LocalOnly;

    #[test]
    fn the_bound_name_registry_is_empty_until_the_session_store_exists() {
        assert!(bound_names().is_empty());
    }

    #[test]
    fn an_undeclared_name_in_an_external_template_fails() {
        let e = run(
            &[
                (step("a", LOCAL, "x", &[], &[]), Some("v")),
                (step("e", EXT, "use {v}", &[], &[]), None),
            ],
            &[],
        )
        .unwrap_err();
        assert!(e.contains("step 'e'") && e.contains("{v}") && e.contains("does not declare"));
        assert!(!e.contains("bound"));
    }

    #[test]
    fn a_declared_name_loads_and_the_default_is_none() {
        let ok = run(
            &[
                (step("a", LOCAL, "x", &[], &[]), Some("v")),
                (step("e", EXT, "use {v}", &["v"], &[]), None),
            ],
            &[],
        );
        assert!(ok.is_ok());
        // An external template with no tokens needs no declaration.
        assert!(run(&[(step("e", EXT, "no tokens at all", &[], &[]), None)], &[]).is_ok());
        // An undeclared SYSTEM token is rejected like any other name.
        let e = run(
            &[(step("e", EXT, "{persona_context}", &[], &[]), None)],
            &[],
        )
        .unwrap_err();
        assert!(e.contains("{persona_context}") && e.contains("does not declare"));
        // A declared system token loads.
        assert!(run(
            &[(step("e", EXT, "{user_input}", &["user_input"], &[]), None)],
            &[]
        )
        .is_ok());
    }

    #[test]
    fn a_field_requirements_name_is_implicitly_declared() {
        assert!(run(
            &[(step("e", EXT, "{job_title}", &[], &["job_title"]), None)],
            &[]
        )
        .is_ok());
        // ...and may be declared again without harm.
        assert!(run(
            &[(
                step("e", EXT, "{job_title}", &["job_title"], &["job_title"]),
                None
            )],
            &[]
        )
        .is_ok());
    }

    #[test]
    fn a_local_step_needs_no_declaration() {
        assert!(run(
            &[
                (step("a", LOCAL, "x", &[], &[]), Some("v")),
                (
                    step("l", LOCAL, "{v} {persona_context} {anything}", &[], &[]),
                    None
                ),
            ],
            &[]
        )
        .is_ok());
    }

    #[test]
    fn a_declared_name_that_nothing_produces_fails() {
        let e = run(&[(step("e", EXT, "x", &["ghost"], &[]), None)], &[]).unwrap_err();
        assert!(e.contains("declared_inputs names 'ghost'"), "{e}");
        // A name produced only by a LATER step is not an earlier output.
        let e = run(
            &[
                (step("e", EXT, "x", &["later"], &[]), None),
                (step("b", LOCAL, "x", &[], &[]), Some("later")),
            ],
            &[],
        )
        .unwrap_err();
        assert!(e.contains("declared_inputs names 'later'"), "{e}");
        // A step's own output is not an earlier output either.
        let e = run(&[(step("e", EXT, "x", &["own"], &[]), Some("own"))], &[]).unwrap_err();
        assert!(e.contains("declared_inputs names 'own'"), "{e}");
        // A local step's declaration is still checked.
        let e = run(&[(step("l", LOCAL, "x", &["ghost"], &[]), None)], &[]).unwrap_err();
        assert!(e.contains("declared_inputs names 'ghost'"), "{e}");
    }

    #[test]
    fn an_undeclared_name_nothing_produces_reports_the_undeclared_message() {
        let e = run(
            &[(step("e", EXT, "{nobody_makes_this}", &[], &[]), None)],
            &[],
        )
        .unwrap_err();
        assert!(
            e.contains("does not declare") && e.contains("{nobody_makes_this}"),
            "{e}"
        );
    }

    #[test]
    fn a_bound_name_fails_unless_declared_and_has_its_own_message() {
        let steps =
            |declared: &[&str]| vec![(step("e", EXT, "turn: {turn_answer}", declared, &[]), None)];
        let e = run(&steps(&[]), &["turn_answer"]).unwrap_err();
        assert!(e.contains("turn-bound or session-bound"), "{e}");
        assert!(e.contains("{turn_answer}"), "{e}");
        // Declared: loads. A bound name is a valid thing to declare.
        assert!(run(&steps(&["turn_answer"]), &["turn_answer"]).is_ok());
        // Without the registry entry it is just an undeclared name.
        let e = run(&steps(&[]), &[]).unwrap_err();
        assert!(
            e.contains("does not declare") && !e.contains("session-bound"),
            "{e}"
        );
        // Declaring a name that is not registered and not produced fails.
        let e = run(&steps(&["turn_answer"]), &[]).unwrap_err();
        assert!(e.contains("declared_inputs names 'turn_answer'"), "{e}");
    }

    #[test]
    fn a_step_after_a_block_cannot_name_the_blocks_variable_undeclared() {
        let mut block = step("qc", LOCAL, "", &[], &[]);
        block.block_stable_id = Some("cb-06".to_owned());
        let rows = |declared: &[&str]| {
            vec![
                (step("draft", LOCAL, "x", &[], &[]), Some("draft_output")),
                (block.clone(), Some("draft_quality_verdict")),
                (
                    step("e", EXT, "verdict: {draft_quality_verdict}", declared, &[]),
                    None,
                ),
            ]
        };
        let e = run(&rows(&[]), &[]).unwrap_err();
        assert!(
            e.contains("{draft_quality_verdict}") && e.contains("does not declare"),
            "{e}"
        );
        assert!(run(&rows(&["draft_quality_verdict"]), &[]).is_ok());
    }
}
