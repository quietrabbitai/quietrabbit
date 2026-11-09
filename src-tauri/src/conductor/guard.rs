// src-tauri/src/conductor/guard.rs
//
// items.id=878 (decisions.id=938, G1): guards on composition rows.
//
// A focus_block_compositions row may carry a typed guard (shared_039.sql:
// guard_var, guard_op, guard_value). When the run reaches the row, execute()
// evaluates the guard over the track's variables, and a row whose guard is
// false is SKIPPED: row order and UNIQUE(focus_id, sequence_index) are
// untouched, and a skipped row emits nothing, writes no checkpoint and records
// nothing on the track.
//
// THE CONDITION LANGUAGE IS CLOSED. Five operators, no expression string, no
// YAML syntax (database-only; the authors are shipped content):
//
//   eq      the variable is set and, trimmed, equals the one literal
//   ne      the variable is set and, trimmed, differs from the literal
//   in      the variable is set and, trimmed, equals one of the literals
//   present the variable is set (its producing row ran and recorded it)
//   absent  the variable is not set
//
// A variable is "set" exactly when TaskTrack holds an output for it, which is
// the same fact collect_output_vars() and TaskTrack::get_output() report: the
// row that names it as output_var executed and recorded a step. An empty model
// reply is therefore a SET value that matches no literal.
//
// A GUARD ON AN ABSENT VARIABLE IS FALSE for eq, ne and in: with nothing to
// compare, no value test holds. That is why ne is not simply "not eq". present
// and absent test the presence itself, so absent is true on an absent variable.
//
// Matching is exact after trimming surrounding whitespace. There is no case
// folding: an author constrains a router's reply in its prompt, and two literals
// that differed only by case would otherwise be ambiguous.
//
// A MODEL VALUE OUTSIDE THE KNOWN LITERALS is one implicit OTHER value. OTHER
// equals no literal, so eq and in are false for it and ne is true.
//
// LOAD VALIDATION (analyze_composition) works on the row shapes, never on model
// output:
//   * a guard variable must be the output_var of an EARLIER row;
//   * paths are enumerated by walking the rows in order and, at each executed
//     row that produces a guard variable, branching over that variable's
//     literals plus OTHER. Only reachable combinations are counted. More than
//     MAX_PATHS paths fails LOAD (an authoring guard, not a runtime cap);
//   * no row may be unreachable (it runs on no path), no path may be empty, and
//     the block-placement rule must hold against the NEXT EXECUTED row of each
//     path;
//   * a router (a variable some eq, ne or in guard reads) needs an explicit
//     fallback: on every path where the router is OTHER, a row guarded on that
//     router must run. Of the five operators only ne and present can be true for
//     OTHER, so one of those is the fallback row.
//
// NOT HERE: back-edges, user turns, session variables, G4 and Tech Support
// (decisions.id=938).

use std::collections::{HashMap, HashSet};

use crate::conductor::types::TaskTrack;

/// Most paths LOAD will enumerate for one Focus. An authoring guard only: there
/// is no runtime turn cap (decisions.id=938, decisions.id=519 stands).
pub const MAX_PATHS: usize = 256;

/// Model value standing for every reply that equals no literal. It holds a NUL,
/// which parse rejects inside a literal, so it can never equal one.
const OTHER: &str = "\u{0}other";

// ---------------------------------------------------------------------------
// Guard
// ---------------------------------------------------------------------------

/// The typed condition. An invalid operator/value pairing cannot be built.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Condition {
    Eq(String),
    Ne(String),
    In(Vec<String>),
    Present,
    Absent,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Guard {
    pub var: String,
    pub condition: Condition,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GuardError {
    #[error("guard is half set (set: {0}): guard_var and guard_op must be set together, and guard_value alone is never valid")]
    HalfSet(String),
    #[error("guard variable name is empty")]
    EmptyVar,
    #[error("unknown guard operator '{0}' (expected eq, ne, in, present or absent)")]
    UnknownOperator(String),
    #[error("guard operator '{0}' needs a guard_value")]
    MissingValue(&'static str),
    #[error("guard operator '{0}' takes no guard_value")]
    UnexpectedValue(&'static str),
    #[error("guard literal is not usable: {0}")]
    BadLiteral(String),
    #[error("guard_value for 'in' is not a JSON array of strings: {0}")]
    BadInList(String),
}

fn check_literal(raw: &str) -> Result<String, GuardError> {
    if raw.contains('\u{0}') {
        return Err(GuardError::BadLiteral("contains a NUL".to_owned()));
    }
    let t = raw.trim();
    if t.is_empty() {
        return Err(GuardError::BadLiteral("empty after trimming".to_owned()));
    }
    Ok(t.to_owned())
}

impl Guard {
    /// Builds a guard from the three nullable columns. `Ok(None)` for an
    /// unguarded row (all three NULL). Every other malformed combination is its
    /// own error, so LOAD can say which part is wrong.
    pub fn from_columns(
        var: Option<&str>,
        op: Option<&str>,
        value: Option<&str>,
    ) -> Result<Option<Guard>, GuardError> {
        let (var, op) = match (var, op) {
            (None, None) if value.is_none() => return Ok(None),
            (Some(v), Some(o)) => (v, o),
            _ => {
                let set = [
                    (var.is_some(), "guard_var"),
                    (op.is_some(), "guard_op"),
                    (value.is_some(), "guard_value"),
                ]
                .iter()
                .filter(|(is_set, _)| *is_set)
                .map(|(_, name)| *name)
                .collect::<Vec<_>>()
                .join(", ");
                return Err(GuardError::HalfSet(set));
            }
        };
        let var = var.trim();
        if var.is_empty() {
            return Err(GuardError::EmptyVar);
        }
        let condition = match op {
            "eq" | "ne" => {
                let raw = value.ok_or(if op == "eq" {
                    GuardError::MissingValue("eq")
                } else {
                    GuardError::MissingValue("ne")
                })?;
                let lit = check_literal(raw)?;
                if op == "eq" {
                    Condition::Eq(lit)
                } else {
                    Condition::Ne(lit)
                }
            }
            "in" => {
                let raw = value.ok_or(GuardError::MissingValue("in"))?;
                let items: Vec<String> =
                    serde_json::from_str(raw).map_err(|e| GuardError::BadInList(e.to_string()))?;
                if items.is_empty() {
                    return Err(GuardError::BadInList("the array is empty".to_owned()));
                }
                let mut seen = HashSet::new();
                let mut lits = Vec::with_capacity(items.len());
                for item in &items {
                    let lit = check_literal(item)?;
                    if !seen.insert(lit.clone()) {
                        return Err(GuardError::BadInList(format!("duplicate literal '{lit}'")));
                    }
                    lits.push(lit);
                }
                Condition::In(lits)
            }
            "present" | "absent" => {
                let name = if op == "present" { "present" } else { "absent" };
                if value.is_some() {
                    return Err(GuardError::UnexpectedValue(name));
                }
                if op == "present" {
                    Condition::Present
                } else {
                    Condition::Absent
                }
            }
            other => return Err(GuardError::UnknownOperator(other.to_owned())),
        };
        Ok(Some(Guard {
            var: var.to_owned(),
            condition,
        }))
    }

    /// Pure evaluation over the track's variables (see the module header for
    /// the table). `vars` maps a variable name to its recorded value.
    pub fn evaluate(&self, vars: &HashMap<String, String>) -> bool {
        let value = vars.get(&self.var).map(|v| v.trim());
        match (&self.condition, value) {
            (Condition::Present, v) => v.is_some(),
            (Condition::Absent, v) => v.is_none(),
            (_, None) => false,
            (Condition::Eq(lit), Some(v)) => v == lit,
            (Condition::Ne(lit), Some(v)) => v != lit,
            (Condition::In(lits), Some(v)) => lits.iter().any(|l| l == v),
        }
    }

    /// The literals this guard names (empty for present and absent).
    fn literals(&self) -> &[String] {
        match &self.condition {
            Condition::Eq(l) | Condition::Ne(l) => std::slice::from_ref(l),
            Condition::In(ls) => ls,
            Condition::Present | Condition::Absent => &[],
        }
    }

    /// True when this guard compares a value (makes its variable a router).
    fn compares_value(&self) -> bool {
        !matches!(self.condition, Condition::Present | Condition::Absent)
    }
}

/// The track's variables, last write wins by name: the same view
/// collect_output_vars() gives a step's prompt. A step with no or an empty
/// output_var contributes nothing.
pub fn track_vars(track: &TaskTrack) -> HashMap<String, String> {
    track
        .steps()
        .iter()
        .filter_map(|s| {
            s.output_var
                .as_ref()
                .filter(|v| !v.is_empty())
                .map(|v| (v.clone(), s.content.clone()))
        })
        .collect()
}

// ---------------------------------------------------------------------------
// LOAD analysis
// ---------------------------------------------------------------------------

/// What the analysis needs to know about one composition row.
pub struct RowShape<'a> {
    pub action_id: &'a str,
    pub guard: Option<&'a Guard>,
    /// The variable the row records when it runs. For a block row this is the
    /// handler's declared output variable, not ActionDefinition.output_var.
    pub output_var: Option<&'a str>,
    pub is_block: bool,
    /// True for an ordinary local step with no handoff: the only kind of row
    /// that may directly follow a block.
    pub local_and_no_handoff: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CompositionError {
    #[error("step '{action_id}': guard variable '{var}' is not the output_var of any earlier step (unknown variable)")]
    UnknownVariable { action_id: String, var: String },
    #[error("more than {MAX_PATHS} execution paths through the guards (authoring cap)")]
    TooManyPaths,
    #[error("step '{action_id}' is unreachable: no path through the guards runs it")]
    Unreachable { action_id: String },
    #[error("a path through the guards runs no step ({path})")]
    EmptyPath { path: String },
    #[error("step '{action_id}': a block step cannot be the final step (its verdict would become the run's output){on_path}")]
    BlockLast { action_id: String, on_path: String },
    #[error("step '{action_id}': a block step cannot come directly before an external or handoff step ('{next}'){on_path}")]
    BlockBeforeExternal {
        action_id: String,
        next: String,
        on_path: String,
    },
    #[error("router variable '{var}' has no fallback row: when it matches none of its literals no step guarded on it runs ({path}). Add a row with an 'ne' or 'present' guard on '{var}'")]
    NoFallback { var: String, path: String },
}

/// One enumerated path: the executed row indices, and the modelled value of
/// every guard variable at the end of it (None = never set on this path).
struct Path {
    rows: Vec<usize>,
    values: Vec<(String, Option<String>)>,
}

fn describe(values: &[(String, Option<String>)]) -> String {
    if values.is_empty() {
        return "no guard variables".to_owned();
    }
    values
        .iter()
        .map(|(var, v)| match v.as_deref() {
            None => format!("{var} unset"),
            Some(OTHER) => format!("{var}=OTHER"),
            Some(x) => format!("{var}='{x}'"),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

struct Walk<'a, 'b> {
    rows: &'b [RowShape<'a>],
    domains: &'b HashMap<String, Vec<String>>,
    guard_vars: &'b [String],
    state: HashMap<String, String>,
    current: Vec<usize>,
    out: Vec<Path>,
}

impl Walk<'_, '_> {
    fn go(&mut self, i: usize) -> Result<(), CompositionError> {
        if i == self.rows.len() {
            if self.out.len() >= MAX_PATHS {
                return Err(CompositionError::TooManyPaths);
            }
            let values = self
                .guard_vars
                .iter()
                .map(|v| (v.clone(), self.state.get(v).cloned()))
                .collect();
            self.out.push(Path {
                rows: self.current.clone(),
                values,
            });
            return Ok(());
        }
        let row = &self.rows[i];
        // The same evaluator the runtime uses, over the modelled values.
        let runs = row.guard.is_none_or(|g| g.evaluate(&self.state));
        if !runs {
            return self.go(i + 1);
        }
        self.current.push(i);
        let produced = row
            .output_var
            .filter(|v| self.domains.contains_key(*v))
            .map(str::to_owned);
        let result = match produced {
            None => self.go(i + 1),
            Some(var) => {
                let previous = self.state.get(&var).cloned();
                let mut res = Ok(());
                let values = self.domains[&var].clone();
                for value in values {
                    self.state.insert(var.clone(), value);
                    res = self.go(i + 1);
                    if res.is_err() {
                        break;
                    }
                }
                match previous {
                    Some(p) => self.state.insert(var, p),
                    None => self.state.remove(&var),
                };
                res
            }
        };
        self.current.pop();
        result
    }
}

/// Validates the guards of one composition and returns the number of paths.
/// An unguarded composition has exactly one path.
pub fn analyze_composition(rows: &[RowShape]) -> Result<usize, CompositionError> {
    // 1. Every guard variable comes from an earlier row's output.
    for (i, row) in rows.iter().enumerate() {
        if let Some(g) = row.guard {
            let earlier = rows[..i].iter().any(|r| r.output_var == Some(&*g.var));
            if !earlier {
                return Err(CompositionError::UnknownVariable {
                    action_id: row.action_id.to_owned(),
                    var: g.var.clone(),
                });
            }
        }
    }

    // 2. Domains: the literals that appear for each guard variable, then OTHER.
    let mut domains: HashMap<String, Vec<String>> = HashMap::new();
    let mut guard_vars: Vec<String> = Vec::new();
    let mut routers: Vec<String> = Vec::new();
    for g in rows.iter().filter_map(|r| r.guard) {
        let d = domains.entry(g.var.clone()).or_insert_with(|| {
            guard_vars.push(g.var.clone());
            Vec::new()
        });
        for lit in g.literals() {
            if !d.contains(lit) {
                d.push(lit.clone());
            }
        }
        if g.compares_value() && !routers.contains(&g.var) {
            routers.push(g.var.clone());
        }
    }
    for d in domains.values_mut() {
        d.push(OTHER.to_owned());
    }

    // 3. Enumerate.
    let mut walk = Walk {
        rows,
        domains: &domains,
        guard_vars: &guard_vars,
        state: HashMap::new(),
        current: Vec::new(),
        out: Vec::new(),
    };
    walk.go(0)?;
    let paths = walk.out;

    // 4. Per-path and whole-composition checks.
    let mut ran = vec![false; rows.len()];
    for p in &paths {
        if p.rows.is_empty() {
            return Err(CompositionError::EmptyPath {
                path: describe(&p.values),
            });
        }
        for &i in &p.rows {
            ran[i] = true;
        }
    }
    if let Some(i) = ran.iter().position(|r| !r) {
        return Err(CompositionError::Unreachable {
            action_id: rows[i].action_id.to_owned(),
        });
    }

    for p in &paths {
        let on_path = if paths.len() > 1 {
            format!(" [path: {}]", describe(&p.values))
        } else {
            String::new()
        };
        for (pos, &i) in p.rows.iter().enumerate() {
            let row = &rows[i];
            if !row.is_block {
                continue;
            }
            let Some(&next_i) = p.rows.get(pos + 1) else {
                return Err(CompositionError::BlockLast {
                    action_id: row.action_id.to_owned(),
                    on_path,
                });
            };
            let next = &rows[next_i];
            if !next.local_and_no_handoff {
                return Err(CompositionError::BlockBeforeExternal {
                    action_id: row.action_id.to_owned(),
                    next: next.action_id.to_owned(),
                    on_path,
                });
            }
        }
        for router in &routers {
            let is_other = p
                .values
                .iter()
                .any(|(v, val)| v == router && val.as_deref() == Some(OTHER));
            if !is_other {
                continue;
            }
            let has_fallback = p
                .rows
                .iter()
                .any(|&i| rows[i].guard.is_some_and(|g| &g.var == router));
            if !has_fallback {
                return Err(CompositionError::NoFallback {
                    var: router.clone(),
                    path: describe(&p.values),
                });
            }
        }
    }

    Ok(paths.len())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conductor::types::TaskStep;

    fn vars(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    fn g(var: &str, op: &str, value: Option<&str>) -> Guard {
        Guard::from_columns(Some(var), Some(op), value)
            .expect("guard parses")
            .expect("guard present")
    }

    // -- parsing ------------------------------------------------------------

    #[test]
    fn all_null_is_unguarded() {
        assert_eq!(Guard::from_columns(None, None, None), Ok(None));
    }

    #[test]
    fn every_operator_parses_to_its_condition() {
        assert_eq!(
            g("v", "eq", Some(" a ")).condition,
            Condition::Eq("a".into())
        );
        assert_eq!(g("v", "ne", Some("a")).condition, Condition::Ne("a".into()));
        assert_eq!(
            g("v", "in", Some(r#"["a", " b "]"#)).condition,
            Condition::In(vec!["a".into(), "b".into()])
        );
        assert_eq!(g("v", "present", None).condition, Condition::Present);
        assert_eq!(g("v", "absent", None).condition, Condition::Absent);
    }

    #[test]
    fn unknown_operator_and_half_set_guards_have_their_own_errors() {
        assert_eq!(
            Guard::from_columns(Some("v"), Some("matches"), Some("x")),
            Err(GuardError::UnknownOperator("matches".into()))
        );
        assert!(matches!(
            Guard::from_columns(Some("v"), None, None),
            Err(GuardError::HalfSet(_))
        ));
        assert!(matches!(
            Guard::from_columns(None, Some("eq"), Some("a")),
            Err(GuardError::HalfSet(_))
        ));
        assert!(matches!(
            Guard::from_columns(None, None, Some("a")),
            Err(GuardError::HalfSet(_))
        ));
        assert_eq!(
            Guard::from_columns(Some("  "), Some("present"), None),
            Err(GuardError::EmptyVar)
        );
    }

    #[test]
    fn value_shape_is_enforced_per_operator() {
        assert!(matches!(
            Guard::from_columns(Some("v"), Some("eq"), None),
            Err(GuardError::MissingValue("eq"))
        ));
        assert!(matches!(
            Guard::from_columns(Some("v"), Some("present"), Some("x")),
            Err(GuardError::UnexpectedValue("present"))
        ));
        assert!(matches!(
            Guard::from_columns(Some("v"), Some("eq"), Some("   ")),
            Err(GuardError::BadLiteral(_))
        ));
        assert!(matches!(
            Guard::from_columns(Some("v"), Some("eq"), Some("a\u{0}b")),
            Err(GuardError::BadLiteral(_))
        ));
        for bad in ["not json", "[]", r#"["a","a"]"#, r#"[1]"#, r#"["  "]"#] {
            assert!(
                Guard::from_columns(Some("v"), Some("in"), Some(bad)).is_err(),
                "{bad}"
            );
        }
    }

    // -- evaluation ---------------------------------------------------------

    #[test]
    fn eq_ne_in_over_a_set_variable() {
        let v = vars(&[("intent", "  billing\n")]);
        assert!(g("intent", "eq", Some("billing")).evaluate(&v));
        assert!(!g("intent", "eq", Some("tech")).evaluate(&v));
        assert!(!g("intent", "ne", Some("billing")).evaluate(&v));
        assert!(g("intent", "ne", Some("tech")).evaluate(&v));
        assert!(g("intent", "in", Some(r#"["tech","billing"]"#)).evaluate(&v));
        assert!(!g("intent", "in", Some(r#"["tech","sales"]"#)).evaluate(&v));
        assert!(g("intent", "present", None).evaluate(&v));
        assert!(!g("intent", "absent", None).evaluate(&v));
    }

    #[test]
    fn matching_is_exact_not_case_folded() {
        let v = vars(&[("intent", "Billing")]);
        assert!(!g("intent", "eq", Some("billing")).evaluate(&v));
    }

    #[test]
    fn an_absent_variable_makes_every_value_test_false() {
        let v = vars(&[]);
        assert!(!g("intent", "eq", Some("a")).evaluate(&v));
        assert!(!g("intent", "ne", Some("a")).evaluate(&v));
        assert!(!g("intent", "in", Some(r#"["a"]"#)).evaluate(&v));
        assert!(!g("intent", "present", None).evaluate(&v));
        assert!(g("intent", "absent", None).evaluate(&v));
    }

    #[test]
    fn an_empty_reply_is_a_set_value_matching_no_literal() {
        let v = vars(&[("intent", "")]);
        assert!(g("intent", "present", None).evaluate(&v));
        assert!(!g("intent", "eq", Some("a")).evaluate(&v));
        assert!(g("intent", "ne", Some("a")).evaluate(&v));
    }

    #[test]
    fn an_out_of_domain_value_is_other() {
        let v = vars(&[("intent", "something the model invented")]);
        assert!(!g("intent", "eq", Some("a")).evaluate(&v));
        assert!(!g("intent", "in", Some(r#"["a","b"]"#)).evaluate(&v));
        assert!(g("intent", "ne", Some("a")).evaluate(&v));
    }

    #[test]
    fn track_vars_is_last_write_wins_and_skips_unnamed_steps() {
        let step = |id: &str, var: Option<&str>, content: &str| TaskStep {
            action_id: id.to_owned(),
            output_var: var.map(str::to_owned),
            content: content.to_owned(),
            sensitivity_severity: 1,
            routing_tier_used: 1,
            personal_flagged: false,
            audit_unavailable: false,
        };
        let mut t = TaskTrack::new();
        t.add_step(step("a", Some("x"), "one"));
        t.add_step(step("b", None, "ignored"));
        t.add_step(step("c", Some(""), "ignored"));
        t.add_step(step("d", Some("x"), "two"));
        let v = track_vars(&t);
        assert_eq!(v.len(), 1);
        assert_eq!(v.get("x").map(String::as_str), Some("two"));
    }

    // -- analysis -----------------------------------------------------------

    struct R {
        id: &'static str,
        guard: Option<Guard>,
        out: Option<&'static str>,
        block: bool,
        local: bool,
    }

    fn r(id: &'static str, guard: Option<Guard>, out: Option<&'static str>) -> R {
        R {
            id,
            guard,
            out,
            block: false,
            local: true,
        }
    }

    fn analyze(rows: &[R]) -> Result<usize, CompositionError> {
        let shapes: Vec<RowShape> = rows
            .iter()
            .map(|x| RowShape {
                action_id: x.id,
                guard: x.guard.as_ref(),
                output_var: x.out,
                is_block: x.block,
                local_and_no_handoff: x.local,
            })
            .collect();
        analyze_composition(&shapes)
    }

    #[test]
    fn an_unguarded_composition_has_one_path() {
        let rows = [r("a", None, Some("x")), r("b", None, None)];
        assert_eq!(analyze(&rows), Ok(1));
    }

    #[test]
    fn a_router_with_a_present_fallback_enumerates_literals_plus_other() {
        let rows = [
            r("route", None, Some("intent")),
            r("billing", Some(g("intent", "eq", Some("billing"))), None),
            r("tech", Some(g("intent", "eq", Some("tech"))), None),
            r("fallback", Some(g("intent", "present", None)), None),
        ];
        assert_eq!(analyze(&rows), Ok(3));
    }

    #[test]
    fn a_guard_on_an_unknown_variable_fails() {
        let rows = [
            r("a", None, Some("x")),
            r("b", Some(g("nope", "present", None)), None),
        ];
        assert!(matches!(
            analyze(&rows),
            Err(CompositionError::UnknownVariable { ref var, .. }) if var == "nope"
        ));
    }

    #[test]
    fn a_guard_variable_must_come_from_an_earlier_row_not_a_later_one() {
        let rows = [
            r("b", Some(g("x", "present", None)), None),
            r("a", None, Some("x")),
        ];
        assert!(matches!(
            analyze(&rows),
            Err(CompositionError::UnknownVariable { .. })
        ));
        // Nor the row's own output.
        let own = [r("a", Some(g("x", "absent", None)), Some("x"))];
        assert!(matches!(
            analyze(&own),
            Err(CompositionError::UnknownVariable { .. })
        ));
    }

    #[test]
    fn an_unreachable_row_fails() {
        // After 'a' runs, x is set, so the absent guard can never hold.
        let rows = [
            r("a", None, Some("x")),
            r("never", Some(g("x", "absent", None)), None),
            r("end", None, None),
        ];
        assert!(matches!(
            analyze(&rows),
            Err(CompositionError::Unreachable { ref action_id }) if action_id == "never"
        ));
    }

    #[test]
    fn a_composition_with_no_rows_is_an_empty_path() {
        // The first row of any non-empty composition is unguarded (a guard on it
        // would have no earlier row to read) so it always runs: the only empty
        // path is the one of a composition with no rows at all.
        assert_eq!(
            analyze(&[]),
            Err(CompositionError::EmptyPath {
                path: "no guard variables".to_owned()
            })
        );
    }

    #[test]
    fn a_router_without_a_fallback_row_fails() {
        let rows = [
            r("route", None, Some("intent")),
            r("billing", Some(g("intent", "eq", Some("billing"))), None),
            r(
                "tech",
                Some(g("intent", "in", Some(r#"["tech","hw"]"#))),
                None,
            ),
            r("end", None, None),
        ];
        assert!(matches!(
            analyze(&rows),
            Err(CompositionError::NoFallback { ref var, .. }) if var == "intent"
        ));
    }

    #[test]
    fn an_ne_row_counts_as_the_fallback() {
        let rows = [
            r("route", None, Some("intent")),
            r("billing", Some(g("intent", "eq", Some("billing"))), None),
            r("other", Some(g("intent", "ne", Some("billing"))), None),
        ];
        assert_eq!(analyze(&rows), Ok(2));
    }

    #[test]
    fn placement_is_checked_against_the_next_executed_row_of_each_path() {
        // The block's next row is guarded. On the path where intent is OTHER it
        // is skipped, so the next executed row is the external 'ext'.
        let block = R {
            id: "blk",
            guard: None,
            out: Some("verdict"),
            block: true,
            local: true,
        };
        let ext = R {
            id: "ext",
            guard: None,
            out: None,
            block: false,
            local: false,
        };
        let rows = [
            r("route", None, Some("intent")),
            r("fb", Some(g("intent", "present", None)), None),
            block,
            r("g", Some(g("intent", "eq", Some("a"))), None),
            ext,
        ];
        match analyze(&rows) {
            Err(CompositionError::BlockBeforeExternal { next, on_path, .. }) => {
                assert_eq!(next, "ext");
                assert!(on_path.contains("intent=OTHER"), "{on_path}");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_block_is_fine_when_every_path_follows_it_with_a_local_row() {
        let block = R {
            id: "blk",
            guard: None,
            out: Some("verdict"),
            block: true,
            local: true,
        };
        let rows = [
            r("route", None, Some("intent")),
            block,
            r("local", Some(g("intent", "eq", Some("a"))), None),
            r("fb", Some(g("intent", "ne", Some("a"))), None),
        ];
        assert_eq!(analyze(&rows), Ok(2));
    }

    #[test]
    fn a_block_cannot_be_last_on_any_path() {
        let block = R {
            id: "blk",
            guard: None,
            out: Some("verdict"),
            block: true,
            local: true,
        };
        let rows = [
            r("route", None, Some("intent")),
            r("a", Some(g("intent", "eq", Some("a"))), None),
            r("fb", Some(g("intent", "ne", Some("a"))), None),
            block,
        ];
        assert!(matches!(
            analyze(&rows),
            Err(CompositionError::BlockLast { .. })
        ));
    }

    #[test]
    fn path_cap_overflow_fails() {
        // Nine independent routers, each one literal plus OTHER: 2^9 = 512 paths.
        let names = ["v0", "v1", "v2", "v3", "v4", "v5", "v6", "v7", "v8"];
        let mut rows: Vec<R> = names.iter().map(|&n| r(n, None, Some(n))).collect();
        for n in names {
            rows.push(r(n, Some(g(n, "eq", Some("x"))), None));
            rows.push(r(n, Some(g(n, "ne", Some("x"))), None));
        }
        assert_eq!(analyze(&rows), Err(CompositionError::TooManyPaths));
    }

    #[test]
    fn exactly_the_cap_is_allowed() {
        // Eight routers, each one literal plus OTHER: 2^8 = 256, exactly the cap.
        let names = ["v0", "v1", "v2", "v3", "v4", "v5", "v6", "v7"];
        let mut rows: Vec<R> = names.iter().map(|&n| r(n, None, Some(n))).collect();
        for n in names {
            rows.push(r(n, Some(g(n, "eq", Some("x"))), None));
            rows.push(r(n, Some(g(n, "ne", Some("x"))), None));
        }
        assert_eq!(analyze(&rows), Ok(MAX_PATHS));
    }

    #[test]
    fn a_skipped_producer_leaves_its_variable_unset_downstream() {
        // 'inner' only runs for intent=a, and sets 'detail'. The row guarded on
        // detail=x therefore runs only on the (a, x) path. Paths: a/x, a/OTHER,
        // OTHER (detail unset, never reached).
        let rows = [
            r("route", None, Some("intent")),
            r("inner", Some(g("intent", "eq", Some("a"))), Some("detail")),
            r("x_only", Some(g("detail", "eq", Some("x"))), None),
            r("fb1", Some(g("intent", "ne", Some("a"))), None),
            r("fb2", Some(g("detail", "present", None)), None),
        ];
        // detail is a router (eq): when detail is OTHER, fb2 (present) runs.
        assert_eq!(analyze(&rows), Ok(3));
    }
}
