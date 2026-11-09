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
// BOUND NAMES (items.id=886, decisions.id=939 section 2): a guard variable may
// also be a bound name (bound_spec.rs), a value read from the session or case
// store at pass start. It is available from row 0, before any row runs, and
// feeds path enumeration with its spec's literals: a Closed domain is exactly
// those literals (write_mode: open, suppressed), an Authored one is the guard
// literals plus OTHER, as for a router. A session name that can be ABSENT adds
// one more state, unset, in which every value test is false, so a Focus that
// routes on locked_intent must have a row that still runs on the first message.
// A row's output_var may not be a bound name.
//
// AND-GUARDS (items.id=889, decisions.id=940 ruling 1, shared_042.sql). A row may
// carry the single guard above AND a list of further clauses (guard_clauses, a
// JSON array of {var, op, value}); the row runs only when EVERY clause holds.
// AND only: no OR, no negation, no expression string, the same five operators.
// The row's whole clause set, single guard included, is capped at
// MAX_GUARD_CLAUSES. Every clause variable must come from an earlier row or be a
// bound name, the path enumeration branches over every clause's variable, and
// the unreachable-row, empty-path and router-fallback checks run over the clause
// SETS (a row whose clauses contradict, such as two different eq literals on one
// variable, runs on no path and fails LOAD as unreachable).
//
// CLOSE-TICKET LINT (items.id=889): a case that reaches a terminal state takes no
// further writes (case_store), so a row that writes the case AFTER one that may
// have closed it would fail the pass at run time. LOAD proves it cannot happen:
// on every path, a case_update whose state may be terminal is the LAST case
// write (case_update, strategy_append, case_link) that runs. "May be terminal"
// is read off the path: a state variable the row's own clauses pin to a
// non-terminal literal is not closing, one pinned to resolved, replaced or closed
// is, and one no clause pins is treated as closing (the cautious side), so an
// author orders it last or pins it.
//
// NOT HERE: back-edges, user turns and Tech Support (decisions.id=938).

use std::collections::{HashMap, HashSet};

use crate::conductor::bound_spec::{self, Literals, Presence};
use crate::conductor::types::TaskTrack;

/// Most clauses one row may carry in total: the single guard (when set) plus the
/// guard_clauses list (items.id=889). Four covers the Tech Support draft (an
/// intent, a ticket state, the write mode and one result variable) and keeps the
/// path enumeration, which multiplies over every variable a clause reads, small.
pub const MAX_GUARD_CLAUSES: usize = 4;

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
    #[error("guard_clauses is not a JSON array of clause objects (var, op, value): {0}")]
    BadClauses(String),
    #[error("guard_clauses is an empty array (leave the column NULL for no clauses)")]
    EmptyClauses,
    #[error("guard_clauses[{index}]: {source}")]
    Clause {
        index: usize,
        #[source]
        source: Box<GuardError>,
    },
    #[error("guard_clauses[{index}] duplicates an earlier clause")]
    DuplicateClause { index: usize },
    #[error("a row may carry at most {MAX_GUARD_CLAUSES} guard clauses in total (the single guard counts as one), this one has {0}")]
    TooManyClauses(usize),
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

/// One clause of the guard_clauses JSON array. Unknown keys are an error (and
/// serde rejects a repeated key), so a typo cannot silently drop a condition.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RawClause {
    var: String,
    op: String,
    #[serde(default)]
    value: Option<serde_json::Value>,
}

impl RawClause {
    fn into_guard(self) -> Result<Guard, GuardError> {
        use serde_json::Value;
        // The value's JSON type is checked per operator HERE, so that an eq
        // literal can never be an array that from_columns would read as text.
        let value: Option<String> = match (self.op.as_str(), self.value) {
            ("eq" | "ne", Some(Value::String(s))) => Some(s),
            ("eq" | "ne", Some(_)) => {
                return Err(GuardError::BadLiteral(
                    "the value of eq and ne is one JSON string".to_owned(),
                ))
            }
            ("in", Some(v @ Value::Array(_))) => Some(v.to_string()),
            ("in", Some(_)) => {
                return Err(GuardError::BadInList(
                    "the value of in is a JSON array of strings".to_owned(),
                ))
            }
            ("present" | "absent", Some(v)) => Some(v.to_string()),
            (_, None) => None,
            (_, Some(_)) => None,
        };
        Guard::from_columns(Some(&self.var), Some(&self.op), value.as_deref())
            .map(|g| g.expect("var and op are set, so a guard is built"))
    }
}

/// Parses the guard_clauses column (shared_042.sql). `None` and an absent column
/// are the empty list. The caller adds the single guard to get the row's whole
/// clause set and checks the total with check_clause_total().
pub fn parse_clauses(text: Option<&str>) -> Result<Vec<Guard>, GuardError> {
    let Some(text) = text else {
        return Ok(Vec::new());
    };
    let raw: Vec<RawClause> =
        serde_json::from_str(text).map_err(|e| GuardError::BadClauses(e.to_string()))?;
    if raw.is_empty() {
        return Err(GuardError::EmptyClauses);
    }
    let mut out: Vec<Guard> = Vec::with_capacity(raw.len());
    for (index, clause) in raw.into_iter().enumerate() {
        let g = clause.into_guard().map_err(|e| GuardError::Clause {
            index,
            source: Box::new(e),
        })?;
        if out.contains(&g) {
            return Err(GuardError::DuplicateClause { index });
        }
        out.push(g);
    }
    Ok(out)
}

/// The cap over a row's whole clause set: the single guard plus the list.
pub fn check_clause_total(single: Option<&Guard>, clauses: &[Guard]) -> Result<(), GuardError> {
    let total = clauses.len() + usize::from(single.is_some());
    if total > MAX_GUARD_CLAUSES {
        return Err(GuardError::TooManyClauses(total));
    }
    if let Some(s) = single {
        if let Some(index) = clauses.iter().position(|c| c == s) {
            return Err(GuardError::DuplicateClause { index });
        }
    }
    Ok(())
}

/// A row's whole clause set in evaluation order: the single guard, then the list.
pub fn clause_set<'a>(single: Option<&'a Guard>, clauses: &'a [Guard]) -> Vec<&'a Guard> {
    single.into_iter().chain(clauses.iter()).collect()
}

/// AND over a clause set: true when every clause holds. The empty set holds, so
/// an unguarded row runs.
pub fn all_hold(clauses: &[&Guard], vars: &HashMap<String, String>) -> bool {
    clauses.iter().all(|g| g.evaluate(vars))
}

/// The first clause of the set that does not hold (for the skip log line).
pub fn first_false<'a>(clauses: &[&'a Guard], vars: &HashMap<String, String>) -> Option<&'a Guard> {
    clauses.iter().copied().find(|g| !g.evaluate(vars))
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
    /// The row's whole clause set, ALL of which must hold for it to run (the
    /// single guard first, then guard_clauses). Empty for an unguarded row.
    pub guards: Vec<&'a Guard>,
    /// The variable the row records when it runs. For a block row this is the
    /// handler's declared output variable, not ActionDefinition.output_var.
    pub output_var: Option<&'a str>,
    pub is_block: bool,
    /// True for an ordinary local step with no handoff: the only kind of row
    /// that may directly follow a block.
    pub local_and_no_handoff: bool,
    /// items.id=887: a store writer. It runs but records nothing on the track,
    /// so it is TRANSPARENT to the block-placement rule (what a block must be
    /// followed by is the next executed row that is not a writer) and does not
    /// by itself make a path non-empty.
    pub transparent: bool,
    /// items.id=887: the variables a store writer reads as arguments (empty for
    /// every other row). On every path that runs the row, each one that some
    /// row produces must have been produced by a row that ran EARLIER on that
    /// path.
    pub reads: Vec<&'a str>,
    /// items.id=889: a store writer that writes the CASE (case_update,
    /// strategy_append or case_link). Used by the close-ticket lint.
    pub case_write: bool,
    /// items.id=889: for a case_update that sets the lifecycle state, the
    /// variable that supplies it. None for every other row.
    pub state_var: Option<&'a str>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CompositionError {
    #[error("step '{action_id}': guard variable '{var}' is not the output_var of any earlier step (unknown variable)")]
    UnknownVariable { action_id: String, var: String },
    #[error("step '{action_id}': output_var '{var}' is the name of a session or case variable read from the store at pass start")]
    BoundNameCollision { action_id: String, var: String },
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
    #[error("step '{action_id}': store writer reads '{var}', but the step that produces it does not run before it on this path ({path})")]
    ReadsUnsetVariable {
        action_id: String,
        var: String,
        path: String,
    },
    #[error("step '{closing}' may close the ticket but step '{later}' writes the case after it: a terminal case takes no further writes, so the closing case_update must be the last case write on every path ({path}). Reorder the rows, or pin the state variable to non-terminal literals with a clause")]
    CloseNotLast {
        closing: String,
        later: String,
        path: String,
    },
    #[error("router variable '{var}' has no fallback row: when it matches none of its literals no step guarded on it runs ({path}). Add a row with an 'ne' or 'present' guard on '{var}'")]
    NoFallback { var: String, path: String },
}

/// A bound name as the analysis sees it (from bound_spec::SPECS).
pub struct BoundVar<'a> {
    pub name: &'a str,
    /// Some: the value is always one of these. None: the guards supply the
    /// literals, plus OTHER.
    pub closed: Option<&'a [&'a str]>,
    /// True when the name can be absent (a session name never written).
    pub optional: bool,
}

/// The registry's names as the analysis sees them.
fn registry_bound() -> Vec<BoundVar<'static>> {
    bound_spec::SPECS
        .iter()
        .map(|s| BoundVar {
            name: s.name,
            closed: match s.literals {
                Literals::Closed(l) => Some(l),
                Literals::Authored => None,
            },
            optional: s.presence == Presence::Optional,
        })
        .collect()
}

/// One enumerated path: the executed row indices, and the modelled value of
/// every guard variable at the end of it (None = never set on this path).
#[derive(Debug, Clone)]
pub struct Path {
    pub rows: Vec<usize>,
    pub values: Vec<(String, Option<String>)>,
}

impl Path {
    /// The modelled guard variables of the path as text, for reports.
    pub fn describe(&self) -> String {
        describe(&self.values)
    }
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
    /// Bound guard variables and the states each can start a pass in
    /// (None = absent).
    bound_start: &'b [(String, Vec<Option<String>>)],
    state: HashMap<String, String>,
    current: Vec<usize>,
    out: Vec<Path>,
}

impl Walk<'_, '_> {
    /// Branch over the starting state of each bound guard variable, then walk
    /// the rows from row 0.
    fn seed(&mut self, k: usize) -> Result<(), CompositionError> {
        let Some((var, states)) = self.bound_start.get(k) else {
            return self.go(0);
        };
        let (var, states) = (var.clone(), states.clone());
        let mut res = Ok(());
        for st in states {
            match st {
                Some(v) => self.state.insert(var.clone(), v),
                None => self.state.remove(&var),
            };
            res = self.seed(k + 1);
            if res.is_err() {
                break;
            }
        }
        self.state.remove(&var);
        res
    }

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
        let runs = row.guards.iter().all(|g| g.evaluate(&self.state));
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
    analyze_composition_paths(rows).map(|p| p.len())
}

/// analyze_composition() returning the enumerated paths themselves (the rows
/// each one runs and its modelled guard variables), for the acceptance report
/// of items.id=889.
pub fn analyze_composition_paths(rows: &[RowShape]) -> Result<Vec<Path>, CompositionError> {
    analyze_paths_with(rows, &registry_bound())
}

/// analyze_composition() over an explicit set of bound names (tests).
pub fn analyze_composition_with(
    rows: &[RowShape],
    bound: &[BoundVar],
) -> Result<usize, CompositionError> {
    analyze_paths_with(rows, bound).map(|p| p.len())
}

fn analyze_paths_with(
    rows: &[RowShape],
    bound: &[BoundVar],
) -> Result<Vec<Path>, CompositionError> {
    // 0. No row's output is a bound name: the seed and the output would share
    // one variable.
    for row in rows {
        if let Some(v) = row
            .output_var
            .filter(|v| bound.iter().any(|b| b.name == *v))
        {
            return Err(CompositionError::BoundNameCollision {
                action_id: row.action_id.to_owned(),
                var: v.to_owned(),
            });
        }
    }

    // 1. Every guard variable comes from an earlier row's output, or is a
    // bound name (available from row 0).
    for (i, row) in rows.iter().enumerate() {
        for g in &row.guards {
            let earlier = rows[..i].iter().any(|r| r.output_var == Some(&*g.var));
            let is_bound = bound.iter().any(|b| b.name == g.var);
            if !earlier && !is_bound {
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
    for g in rows.iter().flat_map(|r| r.guards.iter().copied()) {
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
    // A bound variable with a Closed domain takes exactly those literals (its
    // validator guarantees it), so it has no OTHER. Every other variable is its
    // guard literals plus OTHER.
    let mut bound_start: Vec<(String, Vec<Option<String>>)> = Vec::new();
    for (var, d) in domains.iter_mut() {
        let b = bound.iter().find(|b| b.name == var.as_str());
        match b.and_then(|b| b.closed) {
            Some(closed) => *d = closed.iter().map(|l| (*l).to_owned()).collect(),
            None => d.push(OTHER.to_owned()),
        }
        if let Some(b) = b {
            let mut states: Vec<Option<String>> = d.iter().cloned().map(Some).collect();
            if b.optional {
                states.push(None);
            }
            bound_start.push((var.clone(), states));
        }
    }
    // Deterministic order: the order the variables first appear in guards.
    bound_start.sort_by_key(|(v, _)| guard_vars.iter().position(|g| g == v));

    // 3. Enumerate.
    let mut walk = Walk {
        rows,
        domains: &domains,
        guard_vars: &guard_vars,
        bound_start: &bound_start,
        state: HashMap::new(),
        current: Vec::new(),
        out: Vec::new(),
    };
    walk.seed(0)?;
    let paths = walk.out;

    // 4. Per-path and whole-composition checks.
    let mut ran = vec![false; rows.len()];
    for p in &paths {
        // A path of store writers alone runs no step the user sees: it is as
        // empty as one that runs nothing.
        if p.rows.iter().all(|&i| rows[i].transparent) {
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
            // The next executed row that records something: a store writer in
            // between changes neither the block's verdict nor what follows it.
            let Some(&next_i) = p.rows[pos + 1..].iter().find(|&&j| !rows[j].transparent) else {
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
        // A store writer's arguments: where some row produces the variable, a
        // row that produces it must run earlier on this path.
        for (pos, &i) in p.rows.iter().enumerate() {
            for var in &rows[i].reads {
                let produced_somewhere = rows.iter().any(|r| r.output_var == Some(*var));
                let produced_earlier = p.rows[..pos]
                    .iter()
                    .any(|&j| rows[j].output_var == Some(*var));
                if produced_somewhere && !produced_earlier {
                    return Err(CompositionError::ReadsUnsetVariable {
                        action_id: rows[i].action_id.to_owned(),
                        var: (*var).to_owned(),
                        path: describe(&p.values),
                    });
                }
            }
        }
        check_close_is_last(rows, p)?;
        for router in &routers {
            let is_other = p
                .values
                .iter()
                .any(|(v, val)| v == router && val.as_deref() == Some(OTHER));
            if !is_other {
                continue;
            }
            // A row that runs on this path with a clause on the router: while
            // the router is OTHER only an ne or present clause can hold, so any
            // such row that ran is the fallback.
            let has_fallback = p
                .rows
                .iter()
                .any(|&i| rows[i].guards.iter().any(|g| &g.var == router));
            if !has_fallback {
                return Err(CompositionError::NoFallback {
                    var: router.clone(),
                    path: describe(&p.values),
                });
            }
        }
    }

    Ok(paths)
}

/// True when `value` names a terminal case state (resolved, replaced, closed).
fn is_terminal_state(value: &str) -> bool {
    crate::persistence::case_store::CaseState::parse(value)
        .is_some_and(crate::persistence::case_store::CaseState::is_terminal)
}

/// The close-ticket lint over one path (see the module header): a case_update
/// that may reach a terminal state must be the last case write that runs.
fn check_close_is_last(rows: &[RowShape], p: &Path) -> Result<(), CompositionError> {
    for (pos, &i) in p.rows.iter().enumerate() {
        let Some(state_var) = rows[i].state_var else {
            continue;
        };
        let may_close = match p.values.iter().find(|(v, _)| v == state_var) {
            // A modelled literal: closing exactly when it is a terminal state.
            // OTHER stands for any value no clause names, a terminal state
            // among them, so it may close. An unset variable fails the writer.
            Some((_, Some(v))) => v == OTHER || is_terminal_state(v),
            Some((_, None)) => false,
            // Not pinned by any clause anywhere: the cautious side.
            None => true,
        };
        if !may_close {
            continue;
        }
        if let Some(&j) = p.rows[pos + 1..].iter().find(|&&j| rows[j].case_write) {
            return Err(CompositionError::CloseNotLast {
                closing: rows[i].action_id.to_owned(),
                later: rows[j].action_id.to_owned(),
                path: p.describe(),
            });
        }
    }
    Ok(())
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
                guards: x.guard.as_ref().into_iter().collect(),
                output_var: x.out,
                is_block: x.block,
                local_and_no_handoff: x.local,
                transparent: false,
                reads: Vec::new(),
                case_write: false,
                state_var: None,
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

    // -- bound names (items.id=886) -----------------------------------------

    /// Rows over the REAL registry (analyze_composition, not _with).
    #[test]
    fn a_guard_may_read_a_closed_bound_name_from_row_zero() {
        // write_mode is Closed {open, suppressed} and always present.
        let rows = [
            r("save", Some(g("write_mode", "eq", Some("open"))), None),
            r(
                "skip",
                Some(g("write_mode", "eq", Some("suppressed"))),
                None,
            ),
            r("always", None, None),
        ];
        // Two paths, no OTHER: the value is always one of the two literals.
        assert_eq!(analyze(&rows), Ok(2));
    }

    #[test]
    fn a_closed_bound_name_has_no_other_so_no_fallback_row_is_demanded() {
        let rows = [
            r("a", Some(g("write_mode", "eq", Some("open"))), None),
            r("b", Some(g("write_mode", "ne", Some("open"))), None),
        ];
        assert_eq!(analyze(&rows), Ok(2));
    }

    #[test]
    fn a_guard_literal_outside_a_closed_domain_makes_its_row_unreachable() {
        let rows = [
            r("typo", Some(g("write_mode", "eq", Some("supressed"))), None),
            r("always", None, None),
        ];
        assert!(matches!(
            analyze(&rows),
            Err(CompositionError::Unreachable { ref action_id }) if action_id == "typo"
        ));
    }

    #[test]
    fn an_optional_authored_bound_name_has_unset_literals_and_other() {
        // locked_intent: literals a and b, OTHER, and unset = 4 states. A row
        // that runs on unset (absent guard) and one that runs on OTHER (ne) are
        // both needed, or LOAD fails.
        let rows = [
            r("a", Some(g("locked_intent", "eq", Some("a"))), None),
            r("b", Some(g("locked_intent", "eq", Some("b"))), None),
            r("fallback", Some(g("locked_intent", "ne", Some("a"))), None),
            r("first_turn", Some(g("locked_intent", "absent", None)), None),
        ];
        assert_eq!(analyze(&rows), Ok(4));
    }

    #[test]
    fn routing_only_on_an_optional_name_needs_a_row_that_runs_when_it_is_unset() {
        // With locked_intent unset every value test is false, so ne is false
        // too: nothing runs on the first message.
        let rows = [
            r("a", Some(g("locked_intent", "eq", Some("a"))), None),
            r("fb", Some(g("locked_intent", "ne", Some("a"))), None),
        ];
        assert!(matches!(
            analyze(&rows),
            Err(CompositionError::EmptyPath { ref path }) if path.contains("locked_intent unset")
        ));
    }

    #[test]
    fn a_derived_bound_name_always_exists_so_absent_never_holds() {
        // open_ticket_state is Closed {none, one, many} and never unset.
        let rows = [
            r(
                "none",
                Some(g("open_ticket_state", "eq", Some("none"))),
                None,
            ),
            r("one", Some(g("open_ticket_state", "eq", Some("one"))), None),
            r(
                "many",
                Some(g("open_ticket_state", "eq", Some("many"))),
                None,
            ),
        ];
        assert_eq!(analyze(&rows), Ok(3));
        let dead = [
            r("always", None, None),
            r("never", Some(g("open_ticket_state", "absent", None)), None),
        ];
        assert!(matches!(
            analyze(&dead),
            Err(CompositionError::Unreachable { ref action_id }) if action_id == "never"
        ));
    }

    #[test]
    fn a_row_output_var_that_is_a_bound_name_fails_the_analysis() {
        let rows = [r("a", None, Some("case_context")), r("b", None, None)];
        assert!(matches!(
            analyze(&rows),
            Err(CompositionError::BoundNameCollision { ref var, .. }) if var == "case_context"
        ));
    }

    #[test]
    fn a_bound_guard_variable_combines_with_a_router_in_path_enumeration() {
        // Two states of write_mode times intent (a, OTHER) = 4 paths.
        let rows = [
            r("route", None, Some("intent")),
            r("a_open", Some(g("intent", "eq", Some("a"))), None),
            r("fb", Some(g("intent", "ne", Some("a"))), None),
            r("open", Some(g("write_mode", "eq", Some("open"))), None),
            r(
                "held",
                Some(g("write_mode", "eq", Some("suppressed"))),
                None,
            ),
        ];
        assert_eq!(analyze(&rows), Ok(4));
    }

    // -- items.id=887: store writers in the analysis -------------------------

    fn shape<'a>(
        id: &'a str,
        guard: Option<&'a Guard>,
        out: Option<&'a str>,
        block: bool,
        writer: bool,
        reads: &[&'a str],
    ) -> RowShape<'a> {
        RowShape {
            action_id: id,
            guards: guard.into_iter().collect(),
            output_var: out,
            is_block: block,
            local_and_no_handoff: true,
            transparent: writer,
            reads: reads.to_vec(),
            case_write: false,
            state_var: None,
        }
    }

    #[test]
    fn a_writer_between_a_block_and_the_end_does_not_stand_in_for_an_ordinary_step() {
        let rows = [
            shape("draft", None, Some("d"), false, false, &[]),
            shape("check", None, Some("verdict"), true, false, &[]),
            shape("w", None, None, false, true, &[]),
        ];
        assert!(matches!(
            analyze_composition(&rows),
            Err(CompositionError::BlockLast { .. })
        ));
        let ok = [
            shape("draft", None, Some("d"), false, false, &[]),
            shape("check", None, Some("verdict"), true, false, &[]),
            shape("w", None, None, false, true, &[]),
            shape("tail", None, None, false, false, &[]),
        ];
        assert_eq!(analyze_composition(&ok), Ok(1));
    }

    #[test]
    fn a_path_of_writers_alone_is_an_empty_path() {
        let rows = [shape("w", None, None, false, true, &[])];
        assert!(matches!(
            analyze_composition(&rows),
            Err(CompositionError::EmptyPath { .. })
        ));
    }

    #[test]
    fn a_writers_arguments_must_be_produced_earlier_on_every_path_that_runs_it() {
        let eq_x = g("r", "eq", Some("x"));
        let present_r = g("r", "present", None);
        let bad = [
            shape("route", None, Some("r"), false, false, &[]),
            shape("produce", Some(&eq_x), Some("v"), false, false, &[]),
            shape("w", Some(&present_r), None, false, true, &["v"]),
        ];
        let e = analyze_composition(&bad).unwrap_err();
        assert!(
            matches!(&e, CompositionError::ReadsUnsetVariable { action_id, var, .. }
                if action_id == "w" && var == "v"),
            "{e:?}"
        );
        // The writer guarded as its producer is: every path that runs it has run
        // the producer first. (A fallback row covers r's OTHER value.)
        let ne_x = g("r", "ne", Some("x"));
        let good = [
            shape("route", None, Some("r"), false, false, &[]),
            shape("produce", Some(&eq_x), Some("v"), false, false, &[]),
            shape("w", Some(&eq_x), None, false, true, &["v"]),
            shape("fallback", Some(&ne_x), None, false, false, &[]),
        ];
        assert_eq!(analyze_composition(&good), Ok(2));
        // A variable no row produces (a seeded session name) is not this check's.
        let seeded = [
            shape("a", None, None, false, false, &[]),
            shape("w", None, None, false, true, &["active_case_id"]),
        ];
        assert_eq!(analyze_composition(&seeded), Ok(1));
    }

    #[test]
    fn an_explicit_bound_set_is_honoured_and_path_cap_still_applies() {
        let rows = [
            r("x", Some(g("flag", "eq", Some("on"))), None),
            r("y", Some(g("flag", "ne", Some("on"))), None),
        ];
        let shapes: Vec<RowShape> = rows
            .iter()
            .map(|x| RowShape {
                action_id: x.id,
                guards: x.guard.as_ref().into_iter().collect(),
                output_var: x.out,
                is_block: false,
                local_and_no_handoff: true,
                transparent: false,
                reads: Vec::new(),
                case_write: false,
                state_var: None,
            })
            .collect();
        let bound = [BoundVar {
            name: "flag",
            closed: None,
            optional: true,
        }];
        // on, OTHER, unset: three states, each runs exactly one of x or y
        // (unset runs neither, so it is an empty path).
        assert!(matches!(
            analyze_composition_with(&shapes, &bound),
            Err(CompositionError::EmptyPath { .. })
        ));
        let bound_present = [BoundVar {
            name: "flag",
            closed: Some(&["on", "off"]),
            optional: false,
        }];
        assert_eq!(analyze_composition_with(&shapes, &bound_present), Ok(2));
        // Without the name registered it is an unknown variable.
        assert!(matches!(
            analyze_composition_with(&shapes, &[]),
            Err(CompositionError::UnknownVariable { .. })
        ));
    }

    // -- items.id=889: AND-guards --------------------------------------------

    fn cl(text: &str) -> Result<Vec<Guard>, GuardError> {
        parse_clauses(Some(text))
    }

    #[test]
    fn no_clause_list_is_the_empty_list_and_every_operator_parses() {
        assert_eq!(parse_clauses(None), Ok(Vec::new()));
        let v = cl(r#"[
            {"var":"a","op":"eq","value":" x "},
            {"var":"a","op":"ne","value":"y"},
            {"var":"b","op":"in","value":["p","q"]},
            {"var":"b","op":"present"},
            {"var":"c","op":"absent"}
        ]"#)
        .unwrap();
        assert_eq!(v.len(), 5);
        assert_eq!(v[0].condition, Condition::Eq("x".into()));
        assert_eq!(v[2].condition, Condition::In(vec!["p".into(), "q".into()]));
        assert_eq!(v[4].condition, Condition::Absent);
    }

    #[test]
    fn a_malformed_clause_list_has_its_own_error_per_fault() {
        assert!(matches!(cl("nope"), Err(GuardError::BadClauses(_))));
        assert!(matches!(cl("{}"), Err(GuardError::BadClauses(_))));
        assert_eq!(cl("[]"), Err(GuardError::EmptyClauses));
        // Unknown key, missing key, repeated key: all structural.
        for bad in [
            r#"[{"var":"a","op":"present","x":1}]"#,
            r#"[{"var":"a"}]"#,
            r#"[{"op":"present"}]"#,
            r#"[{"var":"a","var":"b","op":"present"}]"#,
            r#"["a"]"#,
        ] {
            assert!(matches!(cl(bad), Err(GuardError::BadClauses(_))), "{bad}");
        }
        // A fault inside a clause names its index.
        match cl(r#"[{"var":"a","op":"present"},{"var":"a","op":"matches","value":"x"}]"#) {
            Err(GuardError::Clause { index: 1, source }) => {
                assert_eq!(*source, GuardError::UnknownOperator("matches".into()));
            }
            other => panic!("{other:?}"),
        }
        // The JSON type of the value is checked per operator.
        assert!(matches!(
            cl(r#"[{"var":"a","op":"eq","value":["x"]}]"#),
            Err(GuardError::Clause { index: 0, .. })
        ));
        assert!(matches!(
            cl(r#"[{"var":"a","op":"in","value":"x"}]"#),
            Err(GuardError::Clause { index: 0, .. })
        ));
        assert!(matches!(
            cl(r#"[{"var":"a","op":"present","value":"x"}]"#),
            Err(GuardError::Clause { index: 0, .. })
        ));
        assert_eq!(
            cl(r#"[{"var":"a","op":"present"},{"var":"a","op":"present"}]"#),
            Err(GuardError::DuplicateClause { index: 1 })
        );
    }

    #[test]
    fn the_clause_cap_counts_the_single_guard() {
        let four = cl(r#"[
            {"var":"a","op":"present"},{"var":"b","op":"present"},
            {"var":"c","op":"present"},{"var":"d","op":"present"}
        ]"#)
        .unwrap();
        assert_eq!(four.len(), MAX_GUARD_CLAUSES);
        assert_eq!(check_clause_total(None, &four), Ok(()));
        let single = g("e", "present", None);
        assert_eq!(
            check_clause_total(Some(&single), &four),
            Err(GuardError::TooManyClauses(5))
        );
        assert_eq!(
            check_clause_total(Some(&single), &four[..3]),
            Ok(()),
            "single + three is exactly the cap"
        );
        // The single guard repeated in the list is a duplicate.
        assert_eq!(
            check_clause_total(Some(&four[0]), &four[..3]),
            Err(GuardError::DuplicateClause { index: 0 })
        );
    }

    #[test]
    fn a_clause_set_holds_only_when_every_clause_holds() {
        let a = g("intent", "eq", Some("a"));
        let b = g("state", "in", Some(r#"["x","y"]"#));
        let c = g("mode", "present", None);
        let set = [&a, &b, &c];
        let all = vars(&[("intent", "a"), ("state", "y"), ("mode", "m")]);
        assert!(all_hold(&set, &all));
        assert!(first_false(&set, &all).is_none());
        for broken in [
            vars(&[("intent", "b"), ("state", "y"), ("mode", "m")]),
            vars(&[("intent", "a"), ("state", "z"), ("mode", "m")]),
            vars(&[("intent", "a"), ("state", "y")]),
        ] {
            assert!(!all_hold(&set, &broken), "{broken:?}");
        }
        // The first clause that fails is the one reported.
        let broken = vars(&[("intent", "a"), ("state", "z"), ("mode", "m")]);
        assert_eq!(
            first_false(&set, &broken).map(|g| g.var.as_str()),
            Some("state")
        );
        // The empty set holds: an unguarded row runs.
        assert!(all_hold(&[], &vars(&[])));
        // Order of the set is single guard first, then the list.
        let list = [b.clone(), c.clone()];
        let ordered = clause_set(Some(&a), &list);
        assert_eq!(
            ordered.iter().map(|g| g.var.as_str()).collect::<Vec<_>>(),
            ["intent", "state", "mode"]
        );
    }

    struct M {
        id: &'static str,
        guards: Vec<Guard>,
        out: Option<&'static str>,
        case_write: bool,
        state_var: Option<&'static str>,
    }

    fn mrow(id: &'static str, guards: Vec<Guard>, out: Option<&'static str>) -> M {
        M {
            id,
            guards,
            out,
            case_write: false,
            state_var: None,
        }
    }

    fn analyze_m(rows: &[M]) -> Result<Vec<Path>, CompositionError> {
        let shapes: Vec<RowShape> = rows
            .iter()
            .map(|x| RowShape {
                action_id: x.id,
                guards: x.guards.iter().collect(),
                output_var: x.out,
                is_block: false,
                local_and_no_handoff: true,
                transparent: false,
                reads: Vec::new(),
                case_write: x.case_write,
                state_var: x.state_var,
            })
            .collect();
        analyze_composition_paths(&shapes)
    }

    #[test]
    fn two_clauses_are_anded_over_two_routers() {
        // intent (a, OTHER) x stage (x, OTHER): four combinations. The row runs on
        // exactly one of them.
        let rows = [
            mrow("route", vec![], Some("intent")),
            mrow("step", vec![], Some("stage")),
            mrow(
                "both",
                vec![g("intent", "eq", Some("a")), g("stage", "eq", Some("x"))],
                None,
            ),
            mrow("fb", vec![g("intent", "present", None)], None),
            mrow("fb2", vec![g("stage", "present", None)], None),
        ];
        let paths = analyze_m(&rows).unwrap();
        assert_eq!(paths.len(), 4);
        let ran_both = paths.iter().filter(|p| p.rows.contains(&2)).count();
        assert_eq!(ran_both, 1, "only intent=a AND stage=x");
    }

    #[test]
    fn contradicting_clauses_make_a_row_unreachable() {
        let rows = [
            mrow("route", vec![], Some("intent")),
            mrow(
                "never",
                vec![g("intent", "eq", Some("a")), g("intent", "eq", Some("b"))],
                None,
            ),
            mrow("fb", vec![g("intent", "present", None)], None),
        ];
        assert!(matches!(
            analyze_m(&rows),
            Err(CompositionError::Unreachable { ref action_id }) if action_id == "never"
        ));
        // eq and ne of the same literal never hold together either.
        let rows = [
            mrow("route", vec![], Some("intent")),
            mrow(
                "never",
                vec![g("intent", "eq", Some("a")), g("intent", "ne", Some("a"))],
                None,
            ),
            mrow("fb", vec![g("intent", "present", None)], None),
        ];
        assert!(matches!(
            analyze_m(&rows),
            Err(CompositionError::Unreachable { .. })
        ));
    }

    #[test]
    fn an_in_clause_is_an_or_over_one_variable_inside_an_and() {
        let rows = [
            mrow("route", vec![], Some("intent")),
            mrow("step", vec![], Some("stage")),
            mrow(
                "either",
                vec![
                    g("intent", "in", Some(r#"["a","b"]"#)),
                    g("stage", "eq", Some("x")),
                ],
                None,
            ),
            mrow("fb", vec![g("intent", "present", None)], None),
            mrow("fb2", vec![g("stage", "present", None)], None),
        ];
        let paths = analyze_m(&rows).unwrap();
        // intent: a, b, OTHER; stage: x, OTHER = 6 paths, the row on 2 of them.
        assert_eq!(paths.len(), 6);
        assert_eq!(paths.iter().filter(|p| p.rows.contains(&2)).count(), 2);
    }

    #[test]
    fn an_unknown_variable_in_any_clause_fails() {
        let rows = [
            mrow("route", vec![], Some("intent")),
            mrow(
                "bad",
                vec![g("intent", "present", None), g("ghost", "present", None)],
                None,
            ),
        ];
        assert!(matches!(
            analyze_m(&rows),
            Err(CompositionError::UnknownVariable { ref var, .. }) if var == "ghost"
        ));
        // A clause may name a bound name (write_mode) from row 0.
        let rows = [
            mrow("a", vec![], None),
            mrow("b", vec![g("write_mode", "eq", Some("open"))], None),
            mrow("c", vec![g("write_mode", "ne", Some("open"))], None),
        ];
        assert_eq!(analyze_m(&rows).unwrap().len(), 2);
    }

    #[test]
    fn a_fallback_row_with_other_clauses_still_counts_when_it_runs_on_other() {
        let rows = [
            mrow("route", vec![], Some("intent")),
            mrow("a", vec![g("intent", "eq", Some("a"))], None),
            mrow(
                "fb",
                vec![
                    g("intent", "ne", Some("a")),
                    g("write_mode", "present", None),
                ],
                None,
            ),
        ];
        assert_eq!(analyze_m(&rows).unwrap().len(), 4);
        // The same row, but its second clause can never hold together with OTHER
        // routing: no row guarded on the router runs when it is OTHER.
        let rows = [
            mrow("route", vec![], Some("intent")),
            mrow("step", vec![], Some("stage")),
            mrow("a", vec![g("intent", "eq", Some("a"))], None),
            mrow(
                "fb",
                vec![g("intent", "ne", Some("a")), g("stage", "eq", Some("x"))],
                None,
            ),
            mrow("sfb", vec![g("stage", "present", None)], None),
        ];
        // intent=OTHER with stage=OTHER: fb needs stage=x, so no row on intent runs.
        assert!(matches!(
            analyze_m(&rows),
            Err(CompositionError::NoFallback { ref var, .. }) if var == "intent"
        ));
    }

    #[test]
    fn eight_independent_two_literal_routers_still_hit_the_path_cap_exactly() {
        // The cap is on PATHS, not on clauses: clause sets do not change it.
        let names = ["v0", "v1", "v2", "v3", "v4", "v5", "v6", "v7"];
        let mut rows: Vec<M> = names.iter().map(|&n| mrow(n, vec![], Some(n))).collect();
        for n in names {
            rows.push(mrow(n, vec![g(n, "eq", Some("x"))], None));
            rows.push(mrow(n, vec![g(n, "ne", Some("x"))], None));
        }
        assert_eq!(analyze_m(&rows).unwrap().len(), MAX_PATHS);
    }

    // -- the close-ticket lint ------------------------------------------------

    fn writer(
        id: &'static str,
        guards: Vec<Guard>,
        case_write: bool,
        state_var: Option<&'static str>,
    ) -> M {
        M {
            id,
            guards,
            out: None,
            case_write,
            state_var,
        }
    }

    #[test]
    fn a_closing_update_before_another_case_write_is_refused_on_that_path() {
        let terminal = || g("closure", "in", Some(r#"["resolved","closed"]"#));
        let rows = [
            mrow("route", vec![], Some("closure")),
            writer("close", vec![terminal()], true, Some("closure")),
            writer("log", vec![terminal()], true, None),
            mrow("reply", vec![g("closure", "present", None)], None),
        ];
        match analyze_m(&rows) {
            Err(CompositionError::CloseNotLast {
                closing,
                later,
                path,
            }) => {
                assert_eq!((closing.as_str(), later.as_str()), ("close", "log"));
                assert!(path.contains("closure='resolved'") || path.contains("closure='closed'"));
            }
            other => panic!("{other:?}"),
        }
        // Log first, close last: fine.
        let rows = [
            mrow("route", vec![], Some("closure")),
            writer("log", vec![terminal()], true, None),
            writer("close", vec![terminal()], true, Some("closure")),
            mrow("reply", vec![g("closure", "present", None)], None),
        ];
        assert!(analyze_m(&rows).is_ok());
    }

    #[test]
    fn only_a_possibly_terminal_state_counts_as_closing() {
        let open = || g("closure", "in", Some(r#"["open","in-progress"]"#));
        let rows = [
            mrow("route", vec![], Some("closure")),
            writer("update", vec![open()], true, Some("closure")),
            writer("log", vec![open()], true, None),
            mrow("reply", vec![g("closure", "present", None)], None),
        ];
        assert!(analyze_m(&rows).is_ok(), "pinned non-terminal never closes");
        // An ne clause does not pin a state: OTHER may be any terminal value.
        let rows = [
            mrow("route", vec![], Some("closure")),
            writer(
                "update",
                vec![g("closure", "ne", Some("open"))],
                true,
                Some("closure"),
            ),
            writer("log", vec![g("closure", "present", None)], true, None),
            mrow("reply", vec![g("closure", "present", None)], None),
        ];
        assert!(matches!(
            analyze_m(&rows),
            Err(CompositionError::CloseNotLast { .. })
        ));
        // A state variable no clause names is the cautious side.
        let rows = [
            mrow("route", vec![], Some("closure")),
            writer("update", vec![], true, Some("closure")),
            writer("log", vec![], true, None),
            mrow("reply", vec![], None),
        ];
        assert!(matches!(
            analyze_m(&rows),
            Err(CompositionError::CloseNotLast { .. })
        ));
        // A writer that is not a case write never trips it.
        let rows = [
            mrow("route", vec![], Some("closure")),
            writer("update", vec![], true, Some("closure")),
            writer("other", vec![], false, None),
            mrow("reply", vec![], None),
        ];
        assert!(analyze_m(&rows).is_ok());
    }

    #[test]
    fn terminal_states_are_exactly_the_stores_terminal_states() {
        for s in ["resolved", "replaced", "closed"] {
            assert!(is_terminal_state(s), "{s}");
        }
        for s in [
            "open",
            "in-progress",
            "escalated",
            "awaiting-external",
            "nonsense",
            "",
        ] {
            assert!(!is_terminal_state(s), "{s}");
        }
    }
}
