// src-tauri/src/conductor/taint.rs
//
// items.id=886 (decisions.id=939 section 3): the TAINT CLOSURE and the
// per-step token filter that makes it sound.
//
// A bound name (bound_spec.rs) carries content out of the session and case
// stores. The generic extraction pass (extract_candidates) must never see a
// step whose prompt held that content, or a case's symptoms and a device's
// configuration would be offered back as "facts about the user".
//
// THE CLOSURE (pure, computed over the loaded steps; LOAD uses it for the
// declared-inputs rule and the interim external-step invariant, and
// extraction_step_outputs uses the same function to drop steps):
//   * a step is TAINTED when its prompt template names a bound name;
//   * transitively, a step is tainted when its template names a TAINTED
//     VARIABLE, the output_var of a tainted step;
//   * (not in decisions.id=939, added because the design is silent and the
//     omission leaks) a step that names {previous_output} is tainted when any
//     earlier row is tainted, because previous_output is the last recorded
//     step's output whatever produced it. That is an over-approximation of "the
//     step directly before it on this path", which can only drop MORE from
//     extraction, never less. It changes nothing for a Focus with no tainted
//     row.
//   * (items.id=387) in a Focus that has a handoff_ingest row, `user_input` is a
//     tainting name too. The chat window a pass renders ({user_input}) holds the
//     last messages, and after a manual hand-off the newest is a pasted reply from
//     outside. A step that names it could copy that text into its own output, which
//     extraction reads, so such a step is dropped from extraction like any other
//     tainted step. A Focus without a handoff_ingest row is unchanged. The side
//     effect, accepted: while STORE_NAMES_EXTERNAL_ALLOWED was false, an EXTERNAL
//     step of such a Focus could not name {user_input}. Items.id=884 flipped the
//     flag: it may now, behind the outbound review, and a render row that declares
//     it counts as personal on its record (handoff_render::inherit_from_inputs).
// The fixpoint ignores row order on purpose: a template that names a variable
// only a LATER row produces would otherwise escape.
//
// WHY A TOKEN FILTER IS NEEDED TOO. Before items.id=932 render_template() substituted
// every key of the token map into the whole template, repeatedly, including into text
// that an earlier substitution inserted (single-pass substitution closed that). The
// filter stays because it decides which values a step is GIVEN: without it a bound or
// tainted value would be in the map of a step whose template names neither, a step the
// closure rightly treats as clean, and a template that named it would read it.
// StoreTokens::apply therefore (a) removes every bound name and tainted
// variable from a step's token map unless that step's own template names it,
// and (b) inserts a bound name's seed only for a step whose template names it.
// A bound or tainted value thus reaches exactly the steps the closure marked.

use std::collections::{HashMap, HashSet};

use crate::conductor::executor::find_tokens;
use crate::conductor::tokens::ActionDefinition;

/// The result of the closure.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Taint {
    /// action_ids of tainted steps.
    pub actions: HashSet<String>,
    /// Output variables of tainted steps.
    pub vars: HashSet<String>,
    /// items.id=387: `user_input` counts as a tainting name in this Focus (it has a
    /// handoff_ingest row). Always false otherwise.
    pub user_input: bool,
}

/// items.id=387: whether the Focus has a hand-off return row.
fn has_handoff_ingest(steps: &[ActionDefinition]) -> bool {
    steps.iter().any(|s| {
        s.store_op
            .as_ref()
            .is_some_and(|o| o.op == crate::conductor::store_op::Op::HandoffIngest)
    })
}

/// Names a step's prompt template mentions (empty for a pure or model block row,
/// which has no template of its own; a render block row, items.id=387, has one).
fn template_names(step: &ActionDefinition) -> Vec<String> {
    if !step.reads_template() {
        return Vec::new();
    }
    find_tokens(&step.prompt_template)
}

/// The taint closure. `row_vars[i]` is the variable row `i` records when it
/// runs (lifecycle::row_output_vars). `bound` is the bound-name registry.
pub fn taint_closure(
    steps: &[ActionDefinition],
    row_vars: &[Option<String>],
    bound: &[&str],
) -> Taint {
    taint_closure_of(steps, row_vars, bound, true)
}

/// The closure over a chosen set of seed names. `include_user_input` is whether
/// `user_input` is a tainting name in a Focus that has a handoff_ingest row (the
/// store-content closure above). The closure of a CLOSED LIST of names (the device
/// names the outbound review never lets out, declared_inputs.rs) passes false: a
/// pasted reply is not device content, so a step that names `user_input` must not be
/// reported as carrying a device name.
pub fn taint_closure_of(
    steps: &[ActionDefinition],
    row_vars: &[Option<String>],
    bound: &[&str],
    include_user_input: bool,
) -> Taint {
    debug_assert_eq!(steps.len(), row_vars.len());
    let names: Vec<Vec<String>> = steps.iter().map(template_names).collect();
    let mut taint = Taint {
        user_input: include_user_input && has_handoff_ingest(steps),
        ..Taint::default()
    };
    let mut tainted_rows = vec![false; steps.len()];

    loop {
        let mut changed = false;
        for i in 0..steps.len() {
            if tainted_rows[i] {
                continue;
            }
            let names_bound = names[i]
                .iter()
                .any(|n| bound.contains(&n.as_str()) || (taint.user_input && n == "user_input"));
            let names_tainted_var = names[i].iter().any(|n| taint.vars.contains(n));
            let names_previous = names[i].iter().any(|n| n == "previous_output")
                && tainted_rows[..i].iter().any(|t| *t);
            if names_bound || names_tainted_var || names_previous {
                tainted_rows[i] = true;
                taint.actions.insert(steps[i].action_id.clone());
                if let Some(v) = row_vars[i].as_deref().filter(|v| !v.is_empty()) {
                    taint.vars.insert(v.to_owned());
                }
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    taint
}

/// Why a step is tainted, for a LOAD message: the first name in its template
/// that is bound or a tainted variable, or previous_output.
pub fn tainting_name(step: &ActionDefinition, taint: &Taint, bound: &[&str]) -> Option<String> {
    let names = template_names(step);
    names
        .iter()
        .find(|n| {
            bound.contains(&n.as_str())
                || taint.vars.contains(*n)
                || (taint.user_input && *n == "user_input")
        })
        .or_else(|| names.iter().find(|n| *n == "previous_output"))
        .cloned()
}

/// The per-pass store token state a run carries: the seeds read at pass start
/// and the set of names a step may see only if its own template names them.
#[derive(Debug, Clone, Default)]
pub struct StoreTokens {
    /// Seed value by bound name. A session name that is absent has no entry.
    pub seeds: HashMap<String, String>,
    /// Every bound name and every tainted variable of the Focus.
    pub restricted: HashSet<String>,
    /// items.id=922: `user_input` counts as store-backed in this Focus (it has a
    /// handoff_ingest row, so the chat window carries a pasted reply). Set by
    /// seeding::seed_pass from the same closure as `restricted`; false otherwise.
    pub user_input_backed: bool,
}

impl StoreTokens {
    /// items.id=922: whether a declared input is STORE-BACKED, the same set as
    /// handoff_render::store_backed_inputs (a registered bound name, a variable a tainted
    /// step produced, and user_input in a Focus that ingests a pasted reply), read from
    /// the pass's own state so the executor needs no Focus definition.
    pub fn is_store_backed(&self, name: &str) -> bool {
        self.restricted.contains(name) || (self.user_input_backed && name == "user_input")
    }

    /// Adjust one step's token map (see the module header).
    pub fn apply(&self, template: &str, tokens: &mut HashMap<String, String>) {
        if self.restricted.is_empty() && self.seeds.is_empty() {
            return;
        }
        let named: HashSet<String> = find_tokens(template).into_iter().collect();
        tokens.retain(|k, _| !self.restricted.contains(k) || named.contains(k));
        for (k, v) in &self.seeds {
            if named.contains(k) {
                tokens.insert(k.clone(), v.clone());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conductor::tokens::{ExternalAccess, StepType};

    fn step(id: &str, template: &str, out: Option<&str>) -> ActionDefinition {
        ActionDefinition {
            action_id: id.to_owned(),
            display_name: id.to_owned(),
            guide_id: "g".to_owned(),
            task_type: "general".to_owned(),
            routing_tier: ExternalAccess::LocalOnly,
            step_type: StepType::Generate,
            output_var: out.map(str::to_owned),
            prompt_template: template.to_owned(),
            field_requirements: HashMap::new(),
            options_override: HashMap::new(),
            external_access_override: None,
            requires_user_handoff: false,
            revisitable: true,
            block_stable_id: None,
            block_customization: None,
            schedule_trigger: None,
            guard: None,
            declared_inputs: Vec::new(),
            store_op: None,
            and_guards: Vec::new(),
            requirement: None,
        }
    }

    fn closure(steps: &[ActionDefinition], bound: &[&str]) -> Taint {
        let vars: Vec<Option<String>> = steps.iter().map(|s| s.output_var.clone()).collect();
        taint_closure(steps, &vars, bound)
    }

    fn actions(t: &Taint) -> Vec<String> {
        let mut a: Vec<String> = t.actions.iter().cloned().collect();
        a.sort();
        a
    }

    #[test]
    fn a_step_naming_a_bound_name_is_tainted_and_its_variable_with_it() {
        let steps = [
            step("clean", "hello {user_input}", Some("c")),
            step("direct", "ctx: {case_context}", Some("d")),
        ];
        let t = closure(&steps, &["case_context"]);
        assert_eq!(actions(&t), ["direct"]);
        assert_eq!(t.vars, HashSet::from(["d".to_owned()]));
    }

    #[test]
    fn taint_is_transitive_through_output_variables() {
        let steps = [
            step("a", "{case_context}", Some("v1")),
            step("b", "summarise {v1}", Some("v2")),
            step("c", "polish {v2}", Some("v3")),
            step("d", "unrelated {user_input}", Some("v4")),
        ];
        let t = closure(&steps, &["case_context"]);
        assert_eq!(actions(&t), ["a", "b", "c"]);
        assert_eq!(
            t.vars,
            HashSet::from(["v1".to_owned(), "v2".to_owned(), "v3".to_owned()])
        );
    }

    #[test]
    fn taint_does_not_depend_on_row_order() {
        // 'late' names v1 which only the LATER row 'a' produces.
        let steps = [
            step("late", "uses {v1}", Some("v2")),
            step("a", "{locked_intent}", Some("v1")),
            step("after", "uses {v2}", None),
        ];
        let t = closure(&steps, &["locked_intent"]);
        assert_eq!(actions(&t), ["a", "after", "late"]);
    }

    // -- items.id=387: user_input in a Focus with a hand-off return row ---------------

    fn ingest_row() -> ActionDefinition {
        let mut s = step("ingest", "", None);
        s.store_op = crate::conductor::store_op::StoreOp::from_columns(
            Some("handoff_ingest"),
            Some("draft_ref"),
            None,
        )
        .unwrap();
        s
    }

    #[test]
    fn user_input_taints_steps_only_in_a_focus_with_a_hand_off_return_row() {
        let with = [
            ingest_row(),
            step("chat", "answer {user_input}", Some("a")),
            step("after", "refine {a}", Some("b")),
            step("clean", "no names", Some("c")),
        ];
        let t = closure(&with, &[]);
        assert!(t.user_input);
        assert_eq!(actions(&t), ["after", "chat"]);
        assert_eq!(t.vars, HashSet::from(["a".to_owned(), "b".to_owned()]));
        assert_eq!(
            tainting_name(&with[1], &t, &[]).as_deref(),
            Some("user_input")
        );

        // The same steps without the return row are untouched.
        let without = [
            step("chat", "answer {user_input}", Some("a")),
            step("after", "refine {a}", Some("b")),
        ];
        let t = closure(&without, &[]);
        assert!(!t.user_input);
        assert!(t.actions.is_empty() && t.vars.is_empty());
        assert_eq!(tainting_name(&without[0], &t, &[]), None);
    }

    #[test]
    fn the_return_row_itself_and_other_system_tokens_are_not_tainted() {
        let steps = [
            ingest_row(),
            step("p", "ctx {persona_context} prev", Some("x")),
            step("v", "voice {voice_profile}", Some("y")),
        ];
        let t = closure(&steps, &[]);
        assert!(t.actions.is_empty(), "{:?}", t.actions);
    }

    #[test]
    fn an_external_step_naming_user_input_in_such_a_focus_fails_the_interim_lint() {
        let mut ext = step("send", "reply to {user_input}", Some("o"));
        ext.routing_tier = ExternalAccess::AnonymousRequired;
        ext.declared_inputs = vec!["user_input".to_owned()];
        let steps = [ingest_row(), ext.clone()];
        let vars: Vec<Option<String>> = steps.iter().map(|s| s.output_var.clone()).collect();
        // While STORE_NAMES_EXTERNAL_ALLOWED is false, a tainted external step is
        // refused: the paste-bearing window never leaves in a prompt.
        let err = crate::conductor::declared_inputs::lint_with(&steps, &vars, &[], false)
            .expect_err("a tainted external step must fail");
        assert!(err.contains("send"), "{err}");
        // Without the return row, the same external step loads.
        let plain = [ext];
        let plain_vars: Vec<Option<String>> = plain.iter().map(|s| s.output_var.clone()).collect();
        assert!(
            crate::conductor::declared_inputs::lint_with(&plain, &plain_vars, &[], false).is_ok()
        );
    }

    #[test]
    fn previous_output_after_a_tainted_row_is_tainted() {
        let steps = [
            step("a", "{case_context}", Some("v1")),
            step("b", "carry {previous_output}", Some("v2")),
            step("c", "no names at all", None),
        ];
        let t = closure(&steps, &["case_context"]);
        assert_eq!(actions(&t), ["a", "b"]);
        // With no tainted row before it, previous_output is clean.
        let clean = [
            step("x", "plain", Some("p")),
            step("y", "{previous_output}", None),
        ];
        assert!(closure(&clean, &["case_context"]).actions.is_empty());
    }

    #[test]
    fn no_bound_names_in_play_means_nothing_is_tainted() {
        let steps = [
            step("a", "{user_input} {persona_context}", Some("v1")),
            step("b", "{v1} {previous_output}", Some("v2")),
        ];
        assert_eq!(closure(&steps, &["case_context"]), Taint::default());
        assert_eq!(closure(&steps, &[]), Taint::default());
    }

    #[test]
    fn a_block_row_has_no_template_and_is_never_tainted_by_naming() {
        let mut block = step("blk", "{case_context}", Some("verdict"));
        block.block_stable_id = Some("cb-06".to_owned());
        let t = closure(&[block], &["case_context"]);
        assert!(t.actions.is_empty());
    }

    #[test]
    fn tainting_name_reports_the_first_reason() {
        let steps = [
            step("a", "{case_context}", Some("v1")),
            step("b", "x {v1}", None),
            step("c", "{previous_output}", None),
        ];
        let t = closure(&steps, &["case_context"]);
        let b = ["case_context"];
        assert_eq!(
            tainting_name(&steps[0], &t, &b).as_deref(),
            Some("case_context")
        );
        assert_eq!(tainting_name(&steps[1], &t, &b).as_deref(), Some("v1"));
        assert_eq!(
            tainting_name(&steps[2], &t, &b).as_deref(),
            Some("previous_output")
        );
    }

    fn toks(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    fn store(seeds: &[(&str, &str)], restricted: &[&str]) -> StoreTokens {
        StoreTokens {
            seeds: toks(seeds),
            restricted: restricted.iter().map(|s| (*s).to_owned()).collect(),
            user_input_backed: false,
        }
    }

    #[test]
    fn a_seed_reaches_only_a_step_whose_template_names_it() {
        let st = store(&[("case_context", "SEED")], &["case_context"]);
        let mut named = toks(&[("user_input", "u")]);
        st.apply("ctx {case_context}", &mut named);
        assert_eq!(named.get("case_context").map(String::as_str), Some("SEED"));

        let mut unnamed = toks(&[("user_input", "u")]);
        st.apply("only {user_input}", &mut unnamed);
        assert!(!unnamed.contains_key("case_context"));
    }

    #[test]
    fn a_restricted_variable_is_removed_unless_the_template_names_it() {
        let st = store(&[], &["echo1"]);
        let mut a = toks(&[("echo1", "TAINTED"), ("other", "fine")]);
        st.apply("no names {other}", &mut a);
        assert_eq!(a, toks(&[("other", "fine")]));
        let mut b = toks(&[("echo1", "TAINTED"), ("other", "fine")]);
        st.apply("uses {echo1}", &mut b);
        assert_eq!(b.get("echo1").map(String::as_str), Some("TAINTED"));
    }

    #[test]
    fn an_empty_store_tokens_changes_nothing() {
        let st = StoreTokens::default();
        let mut t = toks(&[("a", "1"), ("b", "2")]);
        st.apply("{a}", &mut t);
        assert_eq!(t.len(), 2);
    }

    #[test]
    fn an_absent_session_name_has_no_entry_so_it_renders_as_nothing() {
        let st = store(&[], &["locked_intent"]);
        let mut t = toks(&[]);
        st.apply("intent: {locked_intent}", &mut t);
        assert!(!t.contains_key("locked_intent"));
    }

    /// items.id=891: a step naming either device name is tainted like one naming a
    /// store name, its variable with it, and so is every step downstream.
    #[test]
    fn a_step_naming_a_device_name_is_tainted_with_its_variable_and_downstream() {
        let names = crate::conductor::bound_spec::NAMES;
        for name in ["subject_context", "device_library_brief"] {
            let steps = [
                step("clean", "hello {user_input}", Some("c")),
                step("root", &format!("device: {{{name}}}"), Some("v1")),
                step("down", "from {v1}", Some("v2")),
                step("after", "{previous_output}", Some("v3")),
            ];
            let t = closure(&steps, names);
            assert_eq!(actions(&t), ["after", "down", "root"], "{name}");
            assert!(t.vars.contains("v1") && t.vars.contains("v2"), "{name}");
            assert!(!t.actions.contains("clean"), "{name}");
        }
    }

    /// items.id=895: a step naming an artifact name is tainted like one naming any
    /// store name, its variable with it, and so is every step downstream, so none
    /// of them reaches extraction.
    #[test]
    fn a_step_naming_an_artifact_name_is_tainted_with_its_variable_and_downstream() {
        let names = crate::conductor::bound_spec::NAMES;
        for name in crate::conductor::bound_spec::ARTIFACT_NAMES {
            let steps = [
                step("clean", "hello {user_input}", Some("c")),
                step("root", &format!("brief: {{{name}}}"), Some("v1")),
                step("down", "from {v1}", Some("v2")),
                step("after", "{previous_output}", Some("v3")),
            ];
            let t = closure(&steps, names);
            assert_eq!(actions(&t), ["after", "down", "root"], "{name}");
            assert!(t.vars.contains("v1") && t.vars.contains("v2"), "{name}");
            assert!(!t.actions.contains("clean"), "{name}");
        }
    }

    #[test]
    fn an_artifact_name_is_restricted_to_the_step_that_names_it() {
        for name in crate::conductor::bound_spec::ARTIFACT_NAMES {
            let st = store(&[(name, "ARTIFACT TEXT")], &[name]);
            let mut named = toks(&[]);
            st.apply(&format!("uses {{{name}}}"), &mut named);
            assert_eq!(
                named.get(name).map(String::as_str),
                Some("ARTIFACT TEXT"),
                "{name}"
            );
            let mut unnamed = toks(&[(name, "ARTIFACT TEXT")]);
            st.apply("no names here", &mut unnamed);
            assert!(!unnamed.contains_key(name), "{name}");
        }
    }

    /// items.id=897: a step naming a launch name is tainted like one naming any store
    /// name (its variable with it, and every step downstream), and a launch name is
    /// restricted to the step whose own template names it.
    #[test]
    fn a_step_naming_a_launch_name_is_tainted_and_the_name_is_restricted() {
        let names = crate::conductor::bound_spec::NAMES;
        for name in crate::conductor::bound_spec::LAUNCH_NAMES {
            let steps = [
                step("clean", "hello {user_input}", Some("c")),
                step("root", &format!("mode: {{{name}}}"), Some("v1")),
                step("down", "from {v1}", Some("v2")),
                step("after", "{previous_output}", Some("v3")),
            ];
            let t = closure(&steps, names);
            assert_eq!(actions(&t), ["after", "down", "root"], "{name}");
            assert!(t.vars.contains("v1") && t.vars.contains("v2"), "{name}");
            assert!(!t.actions.contains("clean"), "{name}");

            let st = store(&[(name, "quick")], &[name]);
            let mut named = toks(&[]);
            st.apply(&format!("uses {{{name}}}"), &mut named);
            assert_eq!(named.get(name).map(String::as_str), Some("quick"), "{name}");
            let mut unnamed = toks(&[(name, "quick")]);
            st.apply("no names here", &mut unnamed);
            assert!(!unnamed.contains_key(name), "{name}");
        }
    }
}
