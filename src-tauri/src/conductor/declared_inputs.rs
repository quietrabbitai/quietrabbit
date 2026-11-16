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
//     may not be tainted at all. items.id=884 flipped it to true: an external step
//     may now name a store-backed name it DECLARES, and the outbound review
//     (conductor::step_review) is what stands between that text and the provider.
//   * ONE EXCEPTION THE FLAG NEVER RELAXES (items.id=884, decided by Jason after the
//     flip stage report): the two DEVICE names, DEVICE_NAMES_NEVER_EXTERNAL. An
//     external step may not name or declare either, nor a variable derived from
//     one, whatever the flag says. See that constant for why.
//
// A LOCAL step needs no declaration. If a local step has one anyway its names
// are still checked by (3), so a typo cannot hide until a step's tier is
// raised.
//
// Names are taken from executor::find_tokens, the same extractor the renderer
// strips with, so the lint sees exactly the names the renderer would resolve.

use std::collections::HashSet;

use crate::conductor::executor::find_tokens;
use crate::conductor::taint::{taint_closure, taint_closure_of, tainting_name, Taint};
use crate::conductor::tokens::{is_system_token, ActionDefinition};

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

/// Whether an external step may name or declare a store-backed name (a bound name,
/// a variable a tainted step produced). It was the INTERIM PRIVACY INVARIANT of
/// decisions.id=939 section 5 (items.id=886): store content never left the device
/// in a prompt until the outbound review of a rendered external prompt existed.
/// Items.id=884 built that review (conductor::step_review: one send path, fail
/// closed, the user's answers applied server side) and flipped this to true, the
/// only line that changed here. It lets an external step NAME store-backed names
/// (lint_with); it does not allow hosted model blocks (EXTERNAL_MODEL_BLOCKS_ALLOWED,
/// items.id=913 behind the run plan), and it does not relax
/// DEVICE_NAMES_NEVER_EXTERNAL below.
pub const STORE_NAMES_EXTERNAL_ALLOWED: bool = true;

/// The store-backed names that NO external step may name or declare, whatever
/// STORE_NAMES_EXTERNAL_ALLOWED says (items.id=884 stage 3, items.id=891). A CLOSED
/// list: the two derived DEVICE names (bound_spec: SUBJECT_CONTEXT,
/// DEVICE_LIBRARY_BRIEF), a pin test holds it equal to them.
///
/// WHY. `subject_context` renders EVERY catalogued fact of a device as local text,
/// serial number (credential-flagged), warranty contact and access location included:
/// facts block_data/device_fields.json marks `not_permitted` at tiers 2 and 3, which
/// means never, not even with consent, and omit or range_only for the rest.
/// `device_library_brief` carries only tier-2 `pass` facts but also the user's own
/// device NAMES, and two free-text fields (hardware_specs, known_issues) pass with
/// no secret detector. The outbound review detects personal spans and asks; it is
/// not the catalogue's policy. Letting it stand alone for these names would turn
/// "never" into "one click". decisions.id=513's redaction (a Boundary-surface rule
/// of the visibility function and its flags) is not built in R1 (decisions.id=945),
/// and Tech Support steps are local by default in R1 (decisions.id=937 point 6,
/// 940 ruling 8), so nothing needs these names outside the device today. Opening
/// them later is a deliberate design: a tier-aware render of the catalogue's
/// policies, not a flag.
///
/// The refusal covers a step that names or declares one of them, and a step that
/// names a variable derived from one (a local step that read it and produced an
/// output; see taint::taint_closure_of). It is checked on every Focus, before the
/// declaration rules and whatever the flag says.
pub const DEVICE_NAMES_NEVER_EXTERNAL: &[&str] = &[
    crate::conductor::bound_spec::SUBJECT_CONTEXT,
    crate::conductor::bound_spec::DEVICE_LIBRARY_BRIEF,
];

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
    /// An external step names or declares a DEVICE name, or a variable derived from
    /// one (DEVICE_NAMES_NEVER_EXTERNAL). Whatever STORE_NAMES_EXTERNAL_ALLOWED says.
    DeviceNameInExternalStep { action_id: String, name: String },
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
            Self::DeviceNameInExternalStep { action_id, name } => write!(
                f,
                "step '{action_id}': an external step cannot name or declare '{name}', which \
                 carries a device's catalogued facts (serial number, access location and \
                 warranty contact included). A device name stays on the device, whatever the \
                 outbound review would show the user: the catalogue's tier policies are not \
                 the review's to waive (items.id=884, items.id=891, decisions.id=507)"
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
    let device_taint = taint_closure_of(steps, row_vars, DEVICE_NAMES_NEVER_EXTERNAL, false);
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
        // A pure or model block row has no template and no declaration; it only
        // produces. A render block row (items.id=387) has both, and is external.
        if step.reads_template() {
            // The device names never reach an external step, whatever the flag says,
            // and this is checked FIRST so the refusal is always the explicit one.
            lint_device_names(step, &device_taint).map_err(|e| e.to_string())?;
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

/// DEVICE_NAMES_NEVER_EXTERNAL: an external step may not name or declare a device
/// name, nor a variable derived from one, nor be a step that read one. Independent
/// of STORE_NAMES_EXTERNAL_ALLOWED.
fn lint_device_names(
    step: &ActionDefinition,
    device_taint: &Taint,
) -> Result<(), DeclaredInputsError> {
    if !step.is_external_for_lint() {
        return Ok(());
    }
    let is_device = |n: &str| DEVICE_NAMES_NEVER_EXTERNAL.contains(&n);
    let derived = |n: &str| device_taint.vars.contains(n);
    let offending = step
        .declared_inputs
        .iter()
        .find(|d| is_device(d) || derived(d))
        .cloned()
        .or_else(|| {
            find_tokens(&step.prompt_template)
                .into_iter()
                .find(|n| is_device(n) || derived(n))
        })
        // A step that is tainted by a device name without naming one (it names
        // {previous_output} after a tainted row) still carries device content.
        .or_else(|| {
            device_taint
                .actions
                .contains(&step.action_id)
                .then(|| tainting_name(step, device_taint, DEVICE_NAMES_NEVER_EXTERNAL))
                .flatten()
        });
    match offending {
        Some(name) => Err(DeclaredInputsError::DeviceNameInExternalStep {
            action_id: step.action_id.clone(),
            name,
        }),
        None => Ok(()),
    }
}

/// The store-name rule of the interim invariant: while
/// STORE_NAMES_EXTERNAL_ALLOWED is false, an external step has no store-backed name.
fn lint_store_names(
    step: &ActionDefinition,
    taint: &Taint,
    bound: &[&str],
    bound_like: &HashSet<&str>,
) -> Result<(), DeclaredInputsError> {
    if !step.is_external_for_lint() {
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

    if !step.is_external_for_lint() {
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
    use crate::conductor::tokens::{ExternalAccess, FieldRequirement, StepType};
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
            requirement: None,
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

    /// run() with the store-name invariant CLOSED (STORE_NAMES_EXTERNAL_ALLOWED as it
    /// shipped before items.id=884 flipped it), to keep testing the mechanism the flag
    /// controls. A parameter here, never in production.
    fn run_closed(
        steps: &[(ActionDefinition, Option<&str>)],
        bound: &[&str],
    ) -> Result<(), String> {
        let defs: Vec<ActionDefinition> = steps.iter().map(|(s, _)| s.clone()).collect();
        let vars: Vec<Option<String>> = steps.iter().map(|(_, v)| v.map(str::to_owned)).collect();
        lint_with(&defs, &vars, bound, false)
    }

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

    /// items.id=884: the flag was flipped, the one line that changed in
    /// declared_inputs.rs's rules. Pinned both ways: the constant is true, and the
    /// closed list of device names does NOT depend on it. Changing either needs a
    /// decision (decisions.id=943 point 7 for the flag; this item's stage 3 ruling for
    /// the list).
    #[test]
    fn the_store_name_flag_ships_true_and_the_device_list_does_not_depend_on_it() {
        const { assert!(STORE_NAMES_EXTERNAL_ALLOWED) };
        // The list is exactly the two device names, which are registered bound names.
        assert_eq!(
            DEVICE_NAMES_NEVER_EXTERNAL,
            [
                crate::conductor::bound_spec::SUBJECT_CONTEXT,
                crate::conductor::bound_spec::DEVICE_LIBRARY_BRIEF
            ]
        );
        assert_eq!(
            DEVICE_NAMES_NEVER_EXTERNAL,
            ["subject_context", "device_library_brief"]
        );
        for n in DEVICE_NAMES_NEVER_EXTERNAL {
            assert!(bound_names().contains(n), "{n} is a registered bound name");
        }
        // The refusal runs before, and apart from, the flag: lint_with_bound reads the
        // real constant (true), and the device check is not inside the branch it gates.
        let src = include_str!("declared_inputs.rs");
        let prod = src.split("#[cfg(test)]\nmod tests {").next().unwrap();
        let lint = prod.split("pub(crate) fn lint_with(").nth(1).unwrap();
        let device_at = lint.find("lint_device_names(step, &device_taint)").unwrap();
        let flag_at = lint.find("if !store_names_external_allowed {").unwrap();
        assert!(
            device_at < flag_at,
            "the device refusal is checked before the flag's branch"
        );
        assert!(
            !lint[flag_at..]
                .split("lint_store_names(")
                .next()
                .unwrap()
                .contains("lint_device_names"),
            "the device refusal is not inside the flag's branch"
        );
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
    fn an_external_step_cannot_name_or_declare_a_store_name_while_the_invariant_is_closed() {
        let bound = bound_names();
        // Names it (declared, so the declared-inputs rule is satisfied).
        let e = run_closed(
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
        let e = run_closed(
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
        let e = run_closed(&rows, bound).unwrap_err();
        assert!(e.contains("'echo1'") && e.contains("items.id=884"), "{e}");
        // Transitively: a second hop.
        let rows = vec![
            (step("a", LOCAL, "{locked_intent}", &[], &[]), Some("v1")),
            (step("b", LOCAL, "{v1}", &[], &[]), Some("v2")),
            (step("e", EXT, "{v2}", &["v2"], &[]), None),
        ];
        assert!(run_closed(&rows, bound).unwrap_err().contains("'v2'"));
        // Tainted through {previous_output} with no store name in its template.
        let rows = vec![
            (step("a", LOCAL, "{case_context}", &[], &[]), Some("v1")),
            (
                step("e", EXT, "{previous_output}", &["previous_output"], &[]),
                None,
            ),
        ];
        let e = run_closed(&rows, bound).unwrap_err();
        assert!(
            e.contains("'previous_output'") && e.contains("items.id=884"),
            "{e}"
        );
    }

    /// With the flag as it ships (true) the same declared store names load, and the
    /// declaration rule alone decides: undeclared is still refused.
    #[test]
    fn with_the_flag_true_a_declared_store_name_loads_and_an_undeclared_one_does_not() {
        let bound = bound_names();
        assert!(run(
            &[(
                step("e", EXT, "{case_context}", &["case_context"], &[]),
                None
            )],
            bound
        )
        .is_ok());
        assert!(run(
            &[(step("e", EXT, "no tokens", &["delta_context"], &[]), None)],
            bound
        )
        .is_ok());
        // A tainted variable, transitively, and through {previous_output}.
        let rows: Vec<(ActionDefinition, Option<&str>)> = vec![
            (step("a", LOCAL, "{locked_intent}", &[], &[]), Some("v1")),
            (step("b", LOCAL, "{v1}", &[], &[]), Some("v2")),
            (step("e", EXT, "{v2}", &["v2"], &[]), None),
        ];
        assert!(run(&rows, bound).is_ok());
        let rows: Vec<(ActionDefinition, Option<&str>)> = vec![
            (step("a", LOCAL, "{case_context}", &[], &[]), Some("v1")),
            (
                step("e", EXT, "{previous_output}", &["previous_output"], &[]),
                None,
            ),
        ];
        assert!(run(&rows, bound).is_ok());
        let e = run(&[(step("e", EXT, "{case_context}", &[], &[]), None)], bound).unwrap_err();
        assert!(
            e.contains("session-bound") && e.contains("without declaring"),
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
    fn the_flag_alone_decides_whether_a_declared_store_name_is_allowed() {
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

    /// items.id=891 and items.id=884: the two DEVICE names are refused for an external
    /// step whatever STORE_NAMES_EXTERNAL_ALLOWED says, with a refusal of their own
    /// (DeviceNameInExternalStep), explicit about why. Run with the flag as it ships
    /// (true), with it closed, and open, so no setting relaxes it.
    #[test]
    fn an_external_step_cannot_name_or_declare_a_device_name_whatever_the_flag_says() {
        let bound = bound_names();
        type Runner = fn(&[(ActionDefinition, Option<&str>)], &[&str]) -> Result<(), String>;
        let settings: [(&str, Runner); 3] =
            [("shipped", run), ("closed", run_closed), ("open", run_open)];
        for (setting, runner) in settings {
            for name in DEVICE_NAMES_NEVER_EXTERNAL {
                assert!(bound.contains(name), "{name} is a registered bound name");
                let template = format!("{{{name}}}");
                let explicit = |e: &str| {
                    e.contains("step 'e'")
                        && e.contains(&format!("'{name}'"))
                        && e.contains("device's catalogued facts")
                        && e.contains("serial number")
                        && e.contains("items.id=884")
                };
                // Named and declared.
                let e =
                    runner(&[(step("e", EXT, &template, &[name], &[]), None)], bound).unwrap_err();
                assert!(explicit(&e), "{setting}: {e}");
                // Declared without being named.
                let e = runner(&[(step("e", EXT, "no tokens", &[name], &[]), None)], bound)
                    .unwrap_err();
                assert!(explicit(&e), "{setting}: {e}");
                // Named without being declared: the explicit refusal is the first one.
                let e = runner(&[(step("e", EXT, &template, &[], &[]), None)], bound).unwrap_err();
                assert!(explicit(&e), "{setting}: {e}");
                // A LOCAL step may name it freely.
                assert!(
                    runner(&[(step("a", LOCAL, &template, &[], &[]), Some("v"))], bound).is_ok(),
                    "{setting}"
                );
                // And the name cannot be a row's output variable.
                let e =
                    runner(&[(step("a", LOCAL, "x", &[], &[]), Some(name))], bound).unwrap_err();
                assert!(
                    e.contains(&format!("output_var '{name}'")),
                    "{setting}: {e}"
                );
                // A variable DERIVED from it is refused in an external step too, by the
                // device refusal, and so is a step that follows it by {previous_output}.
                let chain = [
                    (step("a", LOCAL, &template, &[], &[]), Some("v1")),
                    (step("e", EXT, "{v1}", &["v1"], &[]), None),
                ];
                let e = runner(&chain, bound).unwrap_err();
                assert!(
                    e.contains("device's catalogued facts") && e.contains("'v1'"),
                    "{setting}: {e}"
                );
                let via_previous = [
                    (step("a", LOCAL, &template, &[], &[]), Some("v1")),
                    (
                        step("e", EXT, "{previous_output}", &["previous_output"], &[]),
                        None,
                    ),
                ];
                let e = runner(&via_previous, bound).unwrap_err();
                assert!(e.contains("device's catalogued facts"), "{setting}: {e}");
            }
        }
    }

    /// A pasted reply in the turn window is not device content: the device closure
    /// does not count user_input, so a Focus that ingests a reply can still name it in
    /// an external step (the flag is true).
    #[test]
    fn a_pasted_user_input_is_not_a_device_name() {
        let mut ingest = step("ing", LOCAL, "", &[], &[]);
        ingest.store_op = Some(crate::conductor::store_op::StoreOp {
            op: crate::conductor::store_op::Op::HandoffIngest,
            target: Some("reply".to_owned()),
            args: vec![],
        });
        let rows: [(ActionDefinition, Option<&str>); 2] = [
            (ingest, None),
            (step("e", EXT, "{user_input}", &["user_input"], &[]), None),
        ];
        let defs: Vec<ActionDefinition> = rows.iter().map(|(s, _)| s.clone()).collect();
        let vars: Vec<Option<String>> = vec![None, None];
        let taint = taint_closure_of(&defs, &vars, DEVICE_NAMES_NEVER_EXTERNAL, false);
        assert!(!taint.user_input && taint.actions.is_empty() && taint.vars.is_empty());
        assert!(lint_device_names(&defs[1], &taint).is_ok());
    }

    /// items.id=895 (pin test, handoff 632): the four per-pass artifact names are
    /// store-backed bound names. With the store-name invariant CLOSED (the setting
    /// this item flipped; run_closed) an external step may neither name nor declare
    /// one, and the name cannot be a row's output variable. With the flag as it ships
    /// (true) only the declaration rule decides (the next test).
    #[test]
    fn an_external_step_cannot_name_or_declare_an_artifact_name_while_the_invariant_is_closed() {
        let bound = bound_names();
        assert_eq!(
            crate::conductor::bound_spec::ARTIFACT_NAMES.len(),
            4,
            "the pin covers all four"
        );
        for name in crate::conductor::bound_spec::ARTIFACT_NAMES {
            assert!(bound.contains(&name), "{name} is a registered bound name");
            let template = format!("{{{name}}}");
            // Named and declared.
            let e =
                run_closed(&[(step("e", EXT, &template, &[name], &[]), None)], bound).unwrap_err();
            assert!(
                e.contains("step 'e'")
                    && e.contains(&format!("'{name}'"))
                    && e.contains("items.id=884"),
                "{e}"
            );
            // Declared without being named.
            let e = run_closed(&[(step("e", EXT, "no tokens", &[name], &[]), None)], bound)
                .unwrap_err();
            assert!(
                e.contains(&format!("'{name}'")) && e.contains("items.id=884"),
                "{e}"
            );
            // Named without being declared.
            assert!(run_closed(&[(step("e", EXT, &template, &[], &[]), None)], bound).is_err());
            // A LOCAL step may name it freely.
            assert!(
                run_closed(&[(step("a", LOCAL, &template, &[], &[]), Some("v"))], bound).is_ok()
            );
            // A step whose template is tainted by it is itself store-backed: an
            // external step that names the local step's output variable fails too.
            let chain = [
                (step("a", LOCAL, &template, &[], &[]), Some("v1")),
                (step("e", EXT, "{v1}", &["v1"], &[]), None),
            ];
            assert!(
                run_closed(&chain, bound).is_err(),
                "{name}: tainted variable in an external step"
            );
            // And the name cannot be a row's output variable.
            let e =
                run_closed(&[(step("a", LOCAL, "x", &[], &[]), Some(name))], bound).unwrap_err();
            assert!(e.contains(&format!("output_var '{name}'")), "{e}");
        }
    }

    /// With the invariant lifted (a parameter here, never in production) the
    /// declaration rule is what governs an artifact name in an external step.
    #[test]
    fn once_the_invariant_is_lifted_an_external_step_must_declare_an_artifact_name() {
        let bound = bound_names();
        for name in crate::conductor::bound_spec::ARTIFACT_NAMES {
            let template = format!("{{{name}}}");
            assert!(
                run_open(&[(step("e", EXT, &template, &[name], &[]), None)], bound).is_ok(),
                "{name}"
            );
            let e = run_open(&[(step("e", EXT, &template, &[], &[]), None)], bound).unwrap_err();
            assert!(
                e.contains(&format!("'{{{name}}}'")) && e.contains("declared_inputs"),
                "{e}"
            );
        }
    }

    /// items.id=897 (decisions.id=948 point 2): the launch names are ordinary
    /// store-backed bound names for an external step. As the flag ships (true) the
    /// declaration rule decides: an external step must declare one it names, and a
    /// row's output_var may never be one. None of them is a device name, and the
    /// closed device list is unchanged.
    #[test]
    fn an_external_step_must_declare_a_launch_name_it_names_and_none_is_a_device_name() {
        let bound = bound_names();
        for name in crate::conductor::bound_spec::LAUNCH_NAMES {
            assert!(bound.contains(&name), "{name} is a registered bound name");
            assert!(
                !DEVICE_NAMES_NEVER_EXTERNAL.contains(&name),
                "{name} must not be on the closed device list"
            );
            let template = format!("{{{name}}}");
            // Declared and named: allowed once the invariant is lifted, and as it ships.
            assert!(
                run_open(&[(step("e", EXT, &template, &[name], &[]), None)], bound).is_ok(),
                "{name}"
            );
            assert!(
                lint(&[(step("e", EXT, &template, &[name], &[]))], &[None]).is_ok(),
                "{name} declared, as shipped"
            );
            // Named without being declared: refused, naming the declaration rule.
            let e = run_open(&[(step("e", EXT, &template, &[], &[]), None)], bound).unwrap_err();
            assert!(
                e.contains(&format!("'{{{name}}}'")) && e.contains("declared_inputs"),
                "{e}"
            );
            // A LOCAL step may name it freely.
            assert!(run_open(&[(step("a", LOCAL, &template, &[], &[]), Some("v"))], bound).is_ok());
            // And the name cannot be a row's output variable.
            let e = run_open(&[(step("a", LOCAL, "x", &[], &[]), Some(name))], bound).unwrap_err();
            assert!(e.contains(&format!("output_var '{name}'")), "{e}");
        }
        // The closed device list is exactly the two device names (items.id=884) and
        // a launch name does not reach it by derivation: a variable a LOCAL step
        // derives from a launch name is not a device taint.
        assert_eq!(
            DEVICE_NAMES_NEVER_EXTERNAL,
            &["subject_context", "device_library_brief"]
        );
        let chain = [
            (step("a", LOCAL, "{document_type}", &[], &[]), Some("v1")),
            (step("e", EXT, "{v1}", &["v1"], &[]), None),
        ];
        if let Err(m) = run_open(&chain, bound) {
            assert!(
                !m.contains("device"),
                "a launch-derived variable is never refused as a device name: {m}"
            );
        }
    }
}
