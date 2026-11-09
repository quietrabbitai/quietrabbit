// src-tauri/src/conductor/declared_inputs.rs
//
// items.id=879 (decisions.id=937 points 1 and 5, decisions.id=938): the LOAD
// lint for declared inputs. Every EXTERNAL step (routing_tier not LocalOnly)
// lists the variables and tokens its prompt template may name, in its
// `declared_inputs` field (the .focus YAML step and the database customization
// JSON use the same name). The default is NONE, so an external template that
// names anything undeclared fails LOAD.
//
// This is a LOADER LINT ONLY. It changes nothing a prompt renders; Gate 1, 2
// and 3 are unchanged. (items.id=886 adds a per-step token filter in
// taint::StoreTokens, which is about store content reaching an UNNAMED step, not
// about this declaration.) A step's declaration is data that later items read
// (items.id=882 persona_context, 883 history, 884 outbound review).
//
// What a template name is allowed to be, for an external step:
//   * a name in the step's own `declared_inputs`, or
//   * a name in the step's own `field_requirements` (Gate 1's declaration, so it
//     is implicitly declared and is never required twice).
// Anything else is rejected, with a message of its own for each failure:
//   1. a turn-bound or session-bound name that is not declared (see
//      bound_names(), the compiled table in bound_spec.rs; a variable derived
//      from a bound name by the taint closure, taint.rs, counts as one);
//   2. any other undeclared name (this also wins over "nothing produces this
//      name", which is otherwise only a silent strip at render time).
// And a DECLARED name must itself be real, or LOAD fails (3): the output_var of
// an EARLIER row (a block row's output variable counts, so a step after a block
// step cannot name the block's verdict variable unless it declares it), one of
// the five system tokens, a name in the step's own field_requirements (the only
// disclosure fields a step receives: the disclosure buffer is keyed by the
// step), or a registered bound name.
//
// Two more LOAD rules from items.id=886 (decisions.id=939):
//   * a row's output_var must not be a bound name (the seed and the output
//     would share one token);
//   * while STORE_NAMES_EXTERNAL_ALLOWED is false, an external step may not name
//     or declare a store-backed name (a bound name or a tainted variable), and
//     may not be tainted at all.
//
// A LOCAL step needs no declaration. If a local step has one anyway its names
// are still checked by (3), so a typo cannot hide until a step's tier is
// raised.
//
// Names are taken from executor::find_tokens, the same extractor the renderer
// strips with, so the lint sees exactly the names the renderer would resolve.

use std::collections::HashSet;

use crate::conductor::executor::find_tokens;
use crate::conductor::taint::{taint_closure, tainting_name, Taint};
use crate::conductor::tokens::{is_system_token, ActionDefinition, ExternalAccess};

/// Turn-bound and session-bound variable names: the names a pass reads from the
/// session and case stores at pass start (items.id=886, decisions.id=939
/// section 2). They come from the compiled spec table in
/// conductor::bound_spec, which also gives each its source, validator and
/// literal domain. An external step may name one only if it declares it, and
/// while STORE_NAMES_EXTERNAL_ALLOWED is false it may not name or declare one
/// at all.
pub fn bound_names() -> &'static [&'static str] {
    crate::conductor::bound_spec::NAMES
}

/// INTERIM PRIVACY INVARIANT (decisions.id=939 section 5, items.id=886). While
/// this is false, LOAD rejects any EXTERNAL (non-LocalOnly) step that names or
/// declares a store-backed name: a bound name, a variable derived from one (see
/// conductor::taint), or a step that is itself tainted. Store content never
/// leaves the device in a prompt until the outbound review lands. Flip this to
/// true when items.id=884 (the outbound review of a rendered external prompt)
/// lands: it is the only line that changes.
pub const STORE_NAMES_EXTERNAL_ALLOWED: bool = false;

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
    /// A row's output variable has the name of a bound name (items.id=886): the
    /// seed and the output would fight over one token.
    OutputVarShadowsBoundName { action_id: String, name: String },
    /// An external step names or declares a store-backed name while
    /// STORE_NAMES_EXTERNAL_ALLOWED is false (items.id=886, items.id=884).
    StoreNameInExternalStep { action_id: String, name: String },
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
            Self::OutputVarShadowsBoundName { action_id, name } => write!(
                f,
                "step '{action_id}': output_var '{name}' is the name of a session or case \
                 variable a pass reads from the store, so it cannot also be a step's output"
            ),
            Self::StoreNameInExternalStep { action_id, name } => write!(
                f,
                "step '{action_id}': an external step cannot name or declare '{name}', which \
                 carries session or case content, until the outbound review of external \
                 prompts exists (items.id=884)"
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
/// that is not in the real table. STORE_NAMES_EXTERNAL_ALLOWED applies.
pub fn lint_with_bound(
    steps: &[ActionDefinition],
    row_vars: &[Option<String>],
    bound: &[&str],
) -> Result<(), String> {
    lint_with(steps, row_vars, bound, STORE_NAMES_EXTERNAL_ALLOWED)
}

/// The lint with every input explicit. `store_names_external_allowed` is the
/// interim privacy invariant (STORE_NAMES_EXTERNAL_ALLOWED), a parameter so a
/// test can run both settings without flipping the constant.
pub(crate) fn lint_with(
    steps: &[ActionDefinition],
    row_vars: &[Option<String>],
    bound: &[&str],
    store_names_external_allowed: bool,
) -> Result<(), String> {
    debug_assert_eq!(steps.len(), row_vars.len());
    // Taint closure (items.id=886): a variable derived from a bound name counts
    // as a bound name for rule (1) below, so an external step cannot name it
    // undeclared any more than it can name the bound name itself.
    let taint = taint_closure(steps, row_vars, bound);
    let mut bound_like: HashSet<&str> = bound.iter().copied().collect();
    bound_like.extend(taint.vars.iter().map(String::as_str));

    let mut earlier: HashSet<&str> = HashSet::new();
    for (step, var) in steps.iter().zip(row_vars.iter()) {
        let var = var.as_deref().filter(|v| !v.is_empty());
        // A row's output variable must not be a bound name (items.id=886).
        if let Some(v) = var.filter(|v| bound.contains(v)) {
            return Err(DeclaredInputsError::OutputVarShadowsBoundName {
                action_id: step.action_id.clone(),
                name: v.to_owned(),
            }
            .to_string());
        }
        // A block row has no template and no declaration; it only produces.
        if step.block_stable_id.is_none() {
            lint_step(step, &earlier, bound, &bound_like).map_err(|e| e.to_string())?;
            if !store_names_external_allowed {
                lint_store_names(step, &taint, bound, &bound_like).map_err(|e| e.to_string())?;
            }
        }
        if let Some(v) = var {
            earlier.insert(v);
        }
    }
    Ok(())
}

/// The interim invariant: an external step has no store-backed name.
fn lint_store_names(
    step: &ActionDefinition,
    taint: &Taint,
    bound: &[&str],
    bound_like: &HashSet<&str>,
) -> Result<(), DeclaredInputsError> {
    if step.routing_tier == ExternalAccess::LocalOnly {
        return Ok(());
    }
    let offending = step
        .declared_inputs
        .iter()
        .find(|d| bound_like.contains(d.as_str()))
        .cloned()
        .or_else(|| {
            find_tokens(&step.prompt_template)
                .into_iter()
                .find(|n| bound_like.contains(n.as_str()))
        })
        // A step that is tainted without naming a store name (it names
        // {previous_output} after a tainted row) still carries store content.
        .or_else(|| {
            taint
                .actions
                .contains(&step.action_id)
                .then(|| tainting_name(step, taint, bound))
                .flatten()
        });
    match offending {
        Some(name) => Err(DeclaredInputsError::StoreNameInExternalStep {
            action_id: step.action_id.clone(),
            name,
        }),
        None => Ok(()),
    }
}

fn lint_step(
    step: &ActionDefinition,
    earlier: &HashSet<&str>,
    bound: &[&str],
    bound_like: &HashSet<&str>,
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
        return Err(if bound_like.contains(name.as_str()) {
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
            store_op: None,
            and_guards: Vec::new(),
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

    /// run() with the interim external-step invariant switched off, to test the
    /// declared-inputs rules on their own.
    fn run_open(steps: &[(ActionDefinition, Option<&str>)], bound: &[&str]) -> Result<(), String> {
        let defs: Vec<ActionDefinition> = steps.iter().map(|(s, _)| s.clone()).collect();
        let vars: Vec<Option<String>> = steps.iter().map(|(_, v)| v.map(str::to_owned)).collect();
        lint_with(&defs, &vars, bound, true)
    }

    #[test]
    fn the_registry_is_the_spec_tables_names() {
        assert_eq!(bound_names(), crate::conductor::bound_spec::NAMES);
        assert!(!bound_names().is_empty());
    }

    /// The interim invariant ships closed. Flipping it is this one constant.
    #[test]
    fn the_interim_invariant_ships_false() {
        const { assert!(!STORE_NAMES_EXTERNAL_ALLOWED) };
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
        // Declared: loads (with the interim invariant off). A bound name is a
        // valid thing to declare.
        assert!(run_open(&steps(&["turn_answer"]), &["turn_answer"]).is_ok());
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

    // -- items.id=886 -------------------------------------------------------

    #[test]
    fn an_output_var_that_is_a_bound_name_fails() {
        let e = run(
            &[(step("a", LOCAL, "x", &[], &[]), Some("case_context"))],
            bound_names(),
        )
        .unwrap_err();
        assert!(
            e.contains("step 'a'") && e.contains("output_var 'case_context'"),
            "{e}"
        );
        // A block row's output variable counts too.
        let mut block = step("qc", LOCAL, "", &[], &[]);
        block.block_stable_id = Some("cb-06".to_owned());
        let e = run(&[(block, Some("write_mode"))], bound_names()).unwrap_err();
        assert!(e.contains("output_var 'write_mode'"), "{e}");
        // Every registered name is refused, not just two.
        for n in bound_names() {
            assert!(
                run(&[(step("a", LOCAL, "x", &[], &[]), Some(n))], bound_names()).is_err(),
                "{n}"
            );
        }
        // Any other name is fine.
        assert!(run(
            &[(step("a", LOCAL, "x", &[], &[]), Some("case_summary"))],
            bound_names()
        )
        .is_ok());
    }

    /// A variable derived from a bound name counts as a bound name for the
    /// declared-inputs rule: naming it undeclared in an external step gets the
    /// bound-name message.
    #[test]
    fn a_tainted_variable_counts_as_a_bound_name_for_the_declared_inputs_rule() {
        let rows = |declared: &[&str]| {
            vec![
                (step("a", LOCAL, "{case_context}", &[], &[]), Some("echo1")),
                (step("e", EXT, "use {echo1}", declared, &[]), None),
            ]
        };
        let e = run_open(&rows(&[]), bound_names()).unwrap_err();
        assert!(
            e.contains("turn-bound or session-bound") && e.contains("{echo1}"),
            "{e}"
        );
        assert!(run_open(&rows(&["echo1"]), bound_names()).is_ok());
        // A variable no bound name reaches is an ordinary undeclared name.
        let clean = vec![
            (step("a", LOCAL, "{user_input}", &[], &[]), Some("plain")),
            (step("e", EXT, "use {plain}", &[], &[]), None),
        ];
        let e = run_open(&clean, bound_names()).unwrap_err();
        assert!(!e.contains("session-bound"), "{e}");
    }

    #[test]
    fn an_external_step_cannot_name_or_declare_a_store_name_while_the_invariant_holds() {
        let bound = bound_names();
        // Names it (declared, so the declared-inputs rule is satisfied).
        let e = run(
            &[(
                step("e", EXT, "{case_context}", &["case_context"], &[]),
                None,
            )],
            bound,
        )
        .unwrap_err();
        assert!(
            e.contains("step 'e'") && e.contains("'case_context'") && e.contains("items.id=884"),
            "{e}"
        );
        // Declares it without naming it.
        let e = run(
            &[(step("e", EXT, "no tokens", &["delta_context"], &[]), None)],
            bound,
        )
        .unwrap_err();
        assert!(
            e.contains("'delta_context'") && e.contains("items.id=884"),
            "{e}"
        );
        // Names a tainted variable.
        let rows = vec![
            (step("a", LOCAL, "{locked_intent}", &[], &[]), Some("echo1")),
            (step("e", EXT, "{echo1}", &["echo1"], &[]), None),
        ];
        let e = run(&rows, bound).unwrap_err();
        assert!(e.contains("'echo1'") && e.contains("items.id=884"), "{e}");
        // Transitively: a second hop.
        let rows = vec![
            (step("a", LOCAL, "{locked_intent}", &[], &[]), Some("v1")),
            (step("b", LOCAL, "{v1}", &[], &[]), Some("v2")),
            (step("e", EXT, "{v2}", &["v2"], &[]), None),
        ];
        assert!(run(&rows, bound).unwrap_err().contains("'v2'"));
        // Tainted through {previous_output} with no store name in its template.
        let rows = vec![
            (step("a", LOCAL, "{case_context}", &[], &[]), Some("v1")),
            (
                step("e", EXT, "{previous_output}", &["previous_output"], &[]),
                None,
            ),
        ];
        let e = run(&rows, bound).unwrap_err();
        assert!(
            e.contains("'previous_output'") && e.contains("items.id=884"),
            "{e}"
        );
    }

    #[test]
    fn local_steps_and_untainted_external_steps_are_unaffected_by_the_invariant() {
        let bound = bound_names();
        // A LOCAL step may name and declare store names freely.
        assert!(run(
            &[(
                step(
                    "l",
                    LOCAL,
                    "{case_context} {write_mode}",
                    &["case_context"],
                    &[]
                ),
                None
            )],
            bound
        )
        .is_ok());
        // An external step that names nothing store-derived loads, even when
        // the Focus also has a tainted local step.
        let rows = vec![
            (step("a", LOCAL, "{case_context}", &[], &[]), Some("echo1")),
            (step("e", EXT, "{user_input}", &["user_input"], &[]), None),
        ];
        assert!(run(&rows, bound).is_ok());
    }

    #[test]
    fn flipping_the_constant_is_all_it_takes_to_allow_declared_store_names() {
        let defs = vec![step("e", EXT, "{case_context}", &["case_context"], &[])];
        let vars: Vec<Option<String>> = vec![None];
        assert!(lint_with(&defs, &vars, bound_names(), false).is_err());
        assert!(lint_with(&defs, &vars, bound_names(), true).is_ok());
        // Undeclared is still refused when the invariant is off: the
        // declared-inputs rule is separate from the interim invariant.
        let undeclared = vec![step("e", EXT, "{case_context}", &[], &[])];
        let e = lint_with(&undeclared, &vars, bound_names(), true).unwrap_err();
        assert!(e.contains("session-bound"), "{e}");
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
