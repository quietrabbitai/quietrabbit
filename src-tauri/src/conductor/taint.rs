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
// The fixpoint ignores row order on purpose: a template that names a variable
// only a LATER row produces would otherwise escape.
//
// WHY A TOKEN FILTER IS NEEDED TOO. render_template() substitutes every key of
// the token map into the whole template, repeatedly, including into text that
// an earlier substitution inserted. Without a filter, a user turn that reads
// "{case_context}" or "{echo1}" would pull a bound or tainted value into a step
// whose template names neither, a step the closure rightly treats as clean.
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
}

/// Names a step's prompt template mentions (empty for a block row, which has
/// no template).
fn template_names(step: &ActionDefinition) -> Vec<String> {
    if step.block_stable_id.is_some() {
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
    debug_assert_eq!(steps.len(), row_vars.len());
    let names: Vec<Vec<String>> = steps.iter().map(template_names).collect();
    let mut taint = Taint::default();
    let mut tainted_rows = vec![false; steps.len()];

    loop {
        let mut changed = false;
        for i in 0..steps.len() {
            if tainted_rows[i] {
                continue;
            }
            let names_bound = names[i].iter().any(|n| bound.contains(&n.as_str()));
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
        .find(|n| bound.contains(&n.as_str()) || taint.vars.contains(*n))
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
}

impl StoreTokens {
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
}
