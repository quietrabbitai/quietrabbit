// src-tauri/src/conductor/seeding.rs
//
// items.id=886 (decisions.id=939 section 2): PULL-BASED SEEDING of the bound
// names (bound_spec.rs) at the start of a pass.
//
// A pass reads EXACTLY (template tokens UNION declared_inputs UNION guard
// variables) INTERSECT bound_names(), over the Focus's steps, and nothing else.
// A Focus that names no bound name opens no database and needs no chat. The
// values become token-map entries beside the system tokens (taint::StoreTokens,
// applied in executor::render_prompt*), never TaskSteps: a seed is not a step's
// output, so it is never checkpointed, never in focus_run_snapshots and never
// in the extraction input.
//
// ABSENT, DERIVED, FAILED:
//   * a session name with no stored value is ABSENT: no entry, so a guard on it
//     is false and its token renders as nothing (locked_intent, active_case_id);
//   * a derived name ALWAYS exists (an empty string says "nothing");
//   * write_mode always exists too: session_store::write_mode answers open or
//     suppressed from a successful read, and any read error is an error here;
//   * ANY store read error, an unreadable latch, a stored value that fails its
//     validator, or a name that needs a chat when the run has none (write_mode)
//     FAILS THE PASS CLOSED (SeedError). Nothing defaults, and write_mode never
//     defaults to open.
//
// DERIVED NAMES (formats are plain text for a prompt, bounded, brace-free):
//   open_ticket_state   none, one or many open cases in this persona
//   open_tickets_brief  one line per open case (at most BRIEF_CASES)
//   case_context        the active case (active_case_id) and the strategies tried
//   delta_context       config deltas of the active case's primary entity
//
// HANDLES (items.id=889): every ticket line and every delta line starts with
// the row's short opaque HANDLE (T-XXXXXXXX, D-XXXXXXXX, persistence::handles),
// and case_context names its ticket the same way. The internal id is never in
// the text. A model that must pick a ticket or a delta says the handle, and a
// typed store writer (case_select, delta_confirm) resolves it store-side. The
// size caps below are unchanged: a handle adds 10 characters to a line, and a
// cut at the cap can only shorten a handle, which then resolves to nothing.
// case_context and delta_context read the session variable active_case_id
// internally to find the case; that read is part of the derived name, and
// active_case_id itself is seeded only when something asks for it.
//
// ARTIFACTS (items.id=895): content_brief, voice_guidance, validation_report and
// delta_items are session-style names read from pass_artifacts
// (persistence::pass_artifact_store), ABSENT until an artifact_put row wrote the
// kind for this chat and Focus. The text is read ONCE here, at pass start, so a
// value written during a pass is seen by the NEXT pass. They follow every rule
// of a store-backed name: restricted per step (taint::StoreTokens), tainting a
// step that names one (so it stays out of extraction), allowed in an external step
// only when that step declares it (STORE_NAMES_EXTERNAL_ALLOWED, true since
// items.id=884, behind the outbound review), and never the device names. A stored text
// that fails its validator (an over-long or blank row) fails the pass closed.
// Nothing here logs a value.
//
// VALUES ARE CLEANED: '{' and '}' become '(' and ')'. render_template() would
// otherwise strip a stored "{code}" as an unresolved token and substitute a
// stored "{persona_context}" with the persona context.
//
// READS ONLY. Nothing here writes to either store (store writers are
// items.id=887).

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use sqlx::SqliteConnection;

use crate::conductor::bound_spec::{
    self as spec, Presence, Source, ACTIVE_CASE_ID, CASE_CONTEXT, DELTA_CONTEXT,
    DERIVED_TEXT_MAX_BYTES, HANDOFF_PENDING, HANDOFF_RECEIVED, LOCKED_INTENT, OPEN_TICKETS_BRIEF,
    OPEN_TICKET_STATE, WRITE_MODE,
};
use crate::conductor::executor::find_tokens;
use crate::conductor::taint::{taint_closure, StoreTokens};
use crate::conductor::tokens::ActionDefinition;
use crate::persistence::case_store::{self, CaseRecord, EntityConfigDelta};
use crate::persistence::handles;
use crate::persistence::pass_artifact_store::{self, ArtifactKind};
use crate::persistence::session_store::{self, WriteMode};

/// Most open cases open_tickets_brief lists.
pub const BRIEF_CASES: usize = 5;
/// Most config deltas delta_context lists.
pub const DELTA_LINES: usize = 10;
/// Most strategies case_context lists (the newest are kept).
pub const STRATEGY_LINES: usize = 10;

/// Why a pass could not be seeded. The pass FAILS CLOSED on every variant.
/// No variant carries a stored value.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SeedError {
    #[error("'{0}' is read from a chat, and this run has no chat")]
    NoContextKey(&'static str),
    #[error("the session or case store could not be read for '{name}': {message}")]
    Store { name: &'static str, message: String },
    #[error("the stored value of '{0}' is not valid")]
    InvalidValue(&'static str),
    #[error("this persona's store could not be opened: {0}")]
    Open(String),
    /// items.id=897: a launch name reached the store reader. A launch value is
    /// resolved once at LOAD and seeded from that resolution, never read from the
    /// store here (a later stage wires the resolution in), so a pass that asks for
    /// one before then fails closed rather than running without a value.
    #[error(
        "'{0}' is a launch name, resolved when the run starts, and no resolution was supplied"
    )]
    LaunchNotResolved(&'static str),
}

/// The bound names a pass must read, in table order: names in a step's
/// template, its declared_inputs, or a guard variable, intersected with the
/// registry.
pub fn required_names(steps: &[ActionDefinition]) -> Vec<&'static str> {
    let mut wanted: HashSet<String> = HashSet::new();
    for s in steps {
        if s.reads_template() {
            wanted.extend(find_tokens(&s.prompt_template));
        }
        wanted.extend(s.declared_inputs.iter().cloned());
        // items.id=889: every clause variable of the row's AND-guard set.
        for g in s.guards() {
            wanted.insert(g.var.clone());
        }
        // items.id=887: the variables a store writer reads as arguments (a
        // session-bound name among them is seeded at pass start).
        if let Some(op) = &s.store_op {
            wanted.extend(op.arg_vars().map(str::to_owned));
        }
    }
    spec::NAMES
        .iter()
        .copied()
        .filter(|n| wanted.contains(*n))
        .collect()
}

/// Every bound name and every tainted variable of a Focus: the names a step
/// sees only when its own template names them (taint::StoreTokens::restricted).
pub fn restricted_names(
    steps: &[ActionDefinition],
    row_vars: &[Option<String>],
) -> HashSet<String> {
    let mut set: HashSet<String> = spec::NAMES.iter().map(|n| (*n).to_owned()).collect();
    set.extend(taint_closure(steps, row_vars, spec::NAMES).vars);
    set
}

/// Seed one pass: the production entry point FocusRun::execute() calls. Opens
/// the persona's outputs.db only when some bound name is required.
pub async fn seed_pass(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
    context_key: Option<&str>,
    focus_id: &str,
    steps: &[ActionDefinition],
    row_vars: &[Option<String>],
) -> Result<StoreTokens, SeedError> {
    let restricted = restricted_names(steps, row_vars);
    let names = required_names(steps);
    if names.is_empty() {
        return Ok(StoreTokens {
            seeds: HashMap::new(),
            restricted,
        });
    }
    let mut conn = crate::persistence::output_store::open_outputs_db(user_id, persona_id, key_hex)
        .await
        .map_err(|e| SeedError::Open(e.to_string()))?;
    // items.id=891: the two device names read the persona's personal.db as well,
    // and only a pass that names one opens it. read_seeds below handles every
    // other name.
    let (device_names, store_names): (Vec<&'static str>, Vec<&'static str>) = names
        .iter()
        .copied()
        .partition(|n| crate::conductor::device_seeding::is_device_seed(n));
    let mut seeds = if store_names.is_empty() {
        HashMap::new()
    } else {
        read_seeds(&mut conn, context_key, focus_id, &store_names, Utc::now()).await?
    };
    if !device_names.is_empty() {
        let mut personal =
            crate::persistence::personal_store::open_personal_db(user_id, persona_id, key_hex)
                .await
                .map_err(|e| SeedError::Open(e.to_string()))?;
        seeds.extend(
            crate::conductor::device_seeding::read_device_seeds(
                &mut conn,
                &mut personal,
                context_key,
                focus_id,
                &device_names,
            )
            .await?,
        );
    }
    Ok(StoreTokens { seeds, restricted })
}

fn store_err(name: &'static str, e: impl std::fmt::Display) -> SeedError {
    SeedError::Store {
        name,
        message: e.to_string(),
    }
}

/// Read the values of `names` (a subset of the registry). See the module
/// header for absent, derived and failed.
pub async fn read_seeds(
    conn: &mut SqliteConnection,
    context_key: Option<&str>,
    focus_id: &str,
    names: &[&'static str],
    now: DateTime<Utc>,
) -> Result<HashMap<String, String>, SeedError> {
    let wants = |n: &str| names.contains(&n);
    let mut out: HashMap<String, String> = HashMap::new();

    if let Some(n) = names
        .iter()
        .copied()
        .find(|n| spec::spec(n).is_some_and(|s| s.source == Source::Launch))
    {
        return Err(SeedError::LaunchNotResolved(n));
    }

    if wants(WRITE_MODE) {
        let key = context_key.ok_or(SeedError::NoContextKey(WRITE_MODE))?;
        let mode = session_store::write_mode(conn, key)
            .await
            .map_err(|e| store_err(WRITE_MODE, e))?;
        out.insert(
            WRITE_MODE.to_owned(),
            match mode {
                WriteMode::Open => "open",
                WriteMode::Suppressed => "suppressed",
            }
            .to_owned(),
        );
    }

    if wants(LOCKED_INTENT) {
        if let Some(v) = read_session(conn, context_key, focus_id, LOCKED_INTENT).await? {
            out.insert(LOCKED_INTENT.to_owned(), v);
        }
    }

    // items.id=387: the hand-off names, absent until a hand-off starts.
    for name in [HANDOFF_PENDING, HANDOFF_RECEIVED] {
        if wants(name) {
            if let Some(v) = read_session(conn, context_key, focus_id, name).await? {
                out.insert(name.to_owned(), v);
            }
        }
    }

    for kind in ArtifactKind::ALL {
        if wants(kind.as_str()) {
            if let Some(v) = read_artifact(conn, context_key, focus_id, kind).await? {
                out.insert(kind.as_str().to_owned(), v);
            }
        }
    }

    let need_case = wants(CASE_CONTEXT) || wants(DELTA_CONTEXT);
    let mut active_case: Option<String> = None;
    if wants(ACTIVE_CASE_ID) || need_case {
        active_case = read_session(conn, context_key, focus_id, ACTIVE_CASE_ID).await?;
        if wants(ACTIVE_CASE_ID) {
            if let Some(id) = &active_case {
                out.insert(ACTIVE_CASE_ID.to_owned(), id.clone());
            }
        }
    }

    if wants(OPEN_TICKET_STATE) || wants(OPEN_TICKETS_BRIEF) {
        let open = case_store::list_open_cases(conn, focus_id)
            .await
            .map_err(|e| store_err(OPEN_TICKET_STATE, e))?;
        if wants(OPEN_TICKET_STATE) {
            out.insert(
                OPEN_TICKET_STATE.to_owned(),
                match open.len() {
                    0 => "none",
                    1 => "one",
                    _ => "many",
                }
                .to_owned(),
            );
        }
        if wants(OPEN_TICKETS_BRIEF) {
            out.insert(OPEN_TICKETS_BRIEF.to_owned(), render_brief(focus_id, &open));
        }
    }

    if need_case {
        // Only a case this Focus opened is its case: an active_case_id that
        // points at another Focus's case reads as no case.
        let case = match &active_case {
            Some(id) => case_store::get_case(conn, id)
                .await
                .map_err(|e| store_err(CASE_CONTEXT, e))?
                .filter(|c| c.focus_id == focus_id),
            None => None,
        };
        if wants(CASE_CONTEXT) {
            let text = match &case {
                Some(c) => {
                    let strategies = case_store::list_case_strategies(conn, &c.case_id)
                        .await
                        .map_err(|e| store_err(CASE_CONTEXT, e))?;
                    render_case(focus_id, c, &strategies)
                }
                None => String::new(),
            };
            out.insert(CASE_CONTEXT.to_owned(), text);
        }
        if wants(DELTA_CONTEXT) {
            let text = match case.as_ref().and_then(|c| c.primary_entity_id.as_deref()) {
                Some(entity) => {
                    let deltas = case_store::list_config_deltas_for_entity(
                        conn,
                        entity,
                        Some(focus_id),
                        now,
                    )
                    .await
                    .map_err(|e| store_err(DELTA_CONTEXT, e))?;
                    render_deltas(focus_id, &deltas)
                }
                None => String::new(),
            };
            out.insert(DELTA_CONTEXT.to_owned(), text);
        }
    }

    // Every value must satisfy its own spec, and every Always name must exist:
    // a violation fails the pass rather than reaching a prompt.
    for name in names {
        let s = spec::spec(name).expect("required_names only yields registry names");
        match out.get(*name) {
            Some(v) if !(s.validate)(v) => return Err(SeedError::InvalidValue(name)),
            None if s.presence == Presence::Always => return Err(SeedError::InvalidValue(name)),
            _ => {}
        }
    }
    Ok(out)
}

/// One artifact of this Focus in this chat, braces neutralised (clean) and
/// validated. No chat means nothing to read: absent. Any store error, and a text
/// that fails its validator, is an error and fails the pass closed.
async fn read_artifact(
    conn: &mut SqliteConnection,
    context_key: Option<&str>,
    focus_id: &str,
    kind: ArtifactKind,
) -> Result<Option<String>, SeedError> {
    let name = kind.as_str();
    let name: &'static str = spec::ARTIFACT_NAMES
        .iter()
        .copied()
        .find(|n| *n == name)
        .expect("every artifact kind has a bound name");
    debug_assert_eq!(spec::spec(name).map(|s| s.source), Some(Source::Artifact));
    let Some(key) = context_key else {
        return Ok(None);
    };
    let row = pass_artifact_store::get_artifact(conn, key, focus_id, kind)
        .await
        .map_err(|e| store_err(name, e))?;
    let Some(row) = row else { return Ok(None) };
    let text = clean(&row.content);
    let s = spec::spec(name).expect("artifact names are in the registry");
    if (s.validate)(&text) {
        Ok(Some(text))
    } else {
        Err(SeedError::InvalidValue(name))
    }
}

/// One session variable of this Focus in this chat, validated. No chat means
/// nothing to read: absent.
async fn read_session(
    conn: &mut SqliteConnection,
    context_key: Option<&str>,
    focus_id: &str,
    name: &'static str,
) -> Result<Option<String>, SeedError> {
    debug_assert_eq!(spec::spec(name).map(|s| s.source), Some(Source::Session));
    let Some(key) = context_key else {
        return Ok(None);
    };
    let row = session_store::get_session_variable(conn, key, focus_id, name)
        .await
        .map_err(|e| store_err(name, e))?;
    match row {
        None => Ok(None),
        Some(v) => {
            let s = spec::spec(name).expect("session names are in the registry");
            if (s.validate)(&v.value) {
                Ok(Some(v.value))
            } else {
                Err(SeedError::InvalidValue(name))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// Braces would be read as tokens by render_template(). (pub(crate): the device
/// renderers of conductor::device_seeding clean their values the same way.)
pub(crate) fn clean(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '{' => '(',
            '}' => ')',
            '\0' => ' ',
            other => other,
        })
        .collect::<String>()
        .trim()
        .to_owned()
}

/// Cut at DERIVED_TEXT_MAX_BYTES on a character boundary.
pub(crate) fn cap(mut s: String) -> String {
    if s.len() <= DERIVED_TEXT_MAX_BYTES {
        return s;
    }
    let mut end = DERIVED_TEXT_MAX_BYTES;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s.truncate(end);
    s
}

fn label(subject: &str) -> String {
    let c = clean(subject);
    if c.is_empty() {
        "unnamed subject".to_owned()
    } else {
        c
    }
}

fn render_brief(focus_id: &str, open: &[CaseRecord]) -> String {
    let mut lines: Vec<String> = open
        .iter()
        .take(BRIEF_CASES)
        .map(|c| {
            format!(
                "- {}: {} ({}, step {} of 5)",
                handles::case_handle(focus_id, &c.case_id),
                label(&c.subject_label),
                c.state.as_str(),
                c.current_rung
            )
        })
        .collect();
    if open.len() > BRIEF_CASES {
        lines.push(format!("- and {} more", open.len() - BRIEF_CASES));
    }
    cap(lines.join("\n"))
}

fn render_case(focus_id: &str, c: &CaseRecord, strategies: &[case_store::CaseStrategy]) -> String {
    let mut lines = vec![
        format!("Ticket: {}", handles::case_handle(focus_id, &c.case_id)),
        format!("Subject: {}", label(&c.subject_label)),
        format!("State: {} (step {} of 5)", c.state.as_str(), c.current_rung),
    ];
    let symptoms = clean(&c.symptoms);
    if !symptoms.is_empty() {
        lines.push(format!("Symptoms: {symptoms}"));
    }
    if let Some(outcome) = c.outcome.as_deref().map(clean).filter(|o| !o.is_empty()) {
        lines.push(format!("Outcome so far: {outcome}"));
    }
    if !strategies.is_empty() {
        lines.push("Already tried:".to_owned());
        let skip = strategies.len().saturating_sub(STRATEGY_LINES);
        for s in &strategies[skip..] {
            let mut line = format!("- {} -> {}", clean(&s.strategy), clean(&s.outcome));
            if let Some(sig) = s
                .user_signal
                .as_deref()
                .map(clean)
                .filter(|x| !x.is_empty())
            {
                line.push_str(&format!(" (user said: {sig})"));
            }
            lines.push(line);
        }
    }
    cap(lines.join("\n"))
}

fn render_deltas(focus_id: &str, deltas: &[EntityConfigDelta]) -> String {
    let lines: Vec<String> = deltas
        .iter()
        .take(DELTA_LINES)
        .map(|d| {
            format!(
                "- {}: {} [{}, changed {}]",
                handles::delta_handle(focus_id, &d.delta_id),
                clean(&d.description),
                d.effective_status.as_str(),
                clean(&d.changed_at)
            )
        })
        .collect();
    cap(lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conductor::guard::Guard;
    use crate::conductor::tokens::{ExternalAccess, StepType};
    use crate::persistence::case_store::{
        append_case_strategy, open_case, record_config_delta, NewCase, NewConfigDelta, NewStrategy,
    };
    use crate::persistence::session_store::{
        latch_write_suppressed, put_session_variable, test_db::mem_outputs_conn, SessionWrite,
    };

    const CHAT: &str = "chat-seed";
    const FOCUS: &str = "tech-support";

    fn step(id: &str, template: &str) -> ActionDefinition {
        ActionDefinition {
            action_id: id.to_owned(),
            display_name: id.to_owned(),
            guide_id: "g".to_owned(),
            task_type: "general".to_owned(),
            routing_tier: ExternalAccess::LocalOnly,
            step_type: StepType::Generate,
            output_var: None,
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

    async fn put(conn: &mut SqliteConnection, name: &str, value: &str) {
        put_session_variable(
            conn,
            &SessionWrite {
                context_key: CHAT,
                focus_id: FOCUS,
                name,
                value,
                written_by_run: "run-1",
                expected_version: None,
            },
        )
        .await
        .unwrap();
    }

    // -- which names a pass reads ------------------------------------------

    #[test]
    fn required_names_is_templates_union_declared_union_guards_intersect_bound() {
        let mut declared = step("b", "no tokens");
        declared.declared_inputs = vec!["delta_context".to_owned(), "not_bound".to_owned()];
        let mut guarded = step("c", "plain");
        guarded.guard = Guard::from_columns(Some("write_mode"), Some("eq"), Some("open")).unwrap();
        let steps = [
            step("a", "{case_context} and {user_input} and {nothing_bound}"),
            declared,
            guarded,
        ];
        assert_eq!(
            required_names(&steps),
            [WRITE_MODE, CASE_CONTEXT, DELTA_CONTEXT]
        );
        // Nothing bound anywhere: nothing is read.
        assert!(required_names(&[step("x", "{user_input} {v1}")]).is_empty());
    }

    /// items.id=889: every clause of a row's AND-guard set names a variable, so a
    /// bound name that only a clause of the LIST mentions is still read.
    #[test]
    fn a_bound_name_in_a_clause_list_is_a_required_name() {
        let mut row = step("c", "plain");
        row.guard = Guard::from_columns(Some("model_var"), Some("present"), None).unwrap();
        row.and_guards = vec![
            Guard::from_columns(Some("write_mode"), Some("eq"), Some("open"))
                .unwrap()
                .unwrap(),
            Guard::from_columns(Some("open_ticket_state"), Some("ne"), Some("none"))
                .unwrap()
                .unwrap(),
        ];
        assert_eq!(
            required_names(&[row]),
            [WRITE_MODE, OPEN_TICKET_STATE],
            "both list clauses are read, a model variable is not a bound name"
        );
    }

    /// items.id=887: a store writer's argument variables are read at pass start
    /// when they are bound names, and only then.
    #[test]
    fn a_store_writers_bound_arguments_are_required_names() {
        let mut w = step("w", "");
        w.store_op = crate::conductor::store_op::StoreOp::from_columns(
            Some("case_update"),
            None,
            Some(r#"{"case":"active_case_id","outcome":"summary"}"#),
        )
        .unwrap();
        assert_eq!(required_names(&[w]), [ACTIVE_CASE_ID]);
    }

    #[test]
    fn a_block_rows_empty_template_contributes_no_names() {
        let mut block = step("blk", "{case_context}");
        block.block_stable_id = Some("cb-06".to_owned());
        assert!(required_names(&[block]).is_empty());
    }

    #[test]
    fn restricted_names_are_every_bound_name_plus_tainted_variables() {
        let mut a = step("a", "{case_context}");
        a.output_var = Some("echo1".to_owned());
        let steps = [a, step("b", "plain")];
        let vars = vec![Some("echo1".to_owned()), None];
        let r = restricted_names(&steps, &vars);
        for n in spec::NAMES {
            assert!(r.contains(*n), "{n}");
        }
        assert!(r.contains("echo1"));
        assert_eq!(r.len(), spec::NAMES.len() + 1);
    }

    // -- absent / derived / exists -----------------------------------------

    #[tokio::test]
    async fn an_empty_store_gives_absent_session_names_and_present_derived_names() {
        let mut conn = mem_outputs_conn().await;
        // The two device names are read by conductor::device_seeding (they need
        // the persona's personal.db), and the launch names (items.id=897) are
        // seeded from the run's launch resolution, so read_seeds is given every
        // other name.
        let all: Vec<&'static str> = spec::NAMES
            .iter()
            .copied()
            .filter(|n| !crate::conductor::device_seeding::is_device_seed(n))
            .filter(|n| spec::spec(n).is_some_and(|s| s.source != Source::Launch))
            .collect();
        let seeds = read_seeds(&mut conn, Some(CHAT), FOCUS, &all, Utc::now())
            .await
            .unwrap();
        // Absent session names have no entry at all.
        assert!(!seeds.contains_key(LOCKED_INTENT));
        assert!(!seeds.contains_key(ACTIVE_CASE_ID));
        // Derived names and write_mode always exist.
        assert_eq!(seeds[WRITE_MODE], "open");
        assert_eq!(seeds[OPEN_TICKET_STATE], "none");
        assert_eq!(seeds[OPEN_TICKETS_BRIEF], "");
        assert_eq!(seeds[CASE_CONTEXT], "");
        assert_eq!(seeds[DELTA_CONTEXT], "");
    }

    /// items.id=897: a launch name is never read from the store here. A pass that
    /// asks the store reader for one fails closed (until the run's launch resolution
    /// is supplied), whatever else it asks for, and nothing is read first.
    #[tokio::test]
    async fn a_launch_name_asked_of_the_store_reader_fails_closed() {
        let mut conn = mem_outputs_conn().await;
        for name in spec::LAUNCH_NAMES {
            let e = read_seeds(
                &mut conn,
                Some(CHAT),
                FOCUS,
                &[OPEN_TICKET_STATE, name],
                Utc::now(),
            )
            .await
            .unwrap_err();
            assert_eq!(e, SeedError::LaunchNotResolved(name), "{name}");
        }
    }

    #[tokio::test]
    async fn only_the_requested_names_are_read() {
        let mut conn = mem_outputs_conn().await;
        put(&mut conn, LOCKED_INTENT, "billing").await;
        let seeds = read_seeds(
            &mut conn,
            Some(CHAT),
            FOCUS,
            &[OPEN_TICKET_STATE],
            Utc::now(),
        )
        .await
        .unwrap();
        assert_eq!(seeds.len(), 1);
        assert_eq!(seeds[OPEN_TICKET_STATE], "none");
    }

    #[tokio::test]
    async fn write_mode_reads_the_latch() {
        let mut conn = mem_outputs_conn().await;
        latch_write_suppressed(&mut conn, CHAT, FOCUS, "run-1")
            .await
            .unwrap();
        let seeds = read_seeds(&mut conn, Some(CHAT), FOCUS, &[WRITE_MODE], Utc::now())
            .await
            .unwrap();
        assert_eq!(seeds[WRITE_MODE], "suppressed");
        // Another chat is unaffected.
        let other = read_seeds(
            &mut conn,
            Some("chat-other"),
            FOCUS,
            &[WRITE_MODE],
            Utc::now(),
        )
        .await
        .unwrap();
        assert_eq!(other[WRITE_MODE], "open");
    }

    #[tokio::test]
    async fn session_values_are_seeded_for_this_chat_and_focus_only() {
        let mut conn = mem_outputs_conn().await;
        put(&mut conn, LOCKED_INTENT, "wifi_drops").await;
        put(&mut conn, ACTIVE_CASE_ID, "case-9").await;
        let names = [LOCKED_INTENT, ACTIVE_CASE_ID];
        let s = read_seeds(&mut conn, Some(CHAT), FOCUS, &names, Utc::now())
            .await
            .unwrap();
        assert_eq!(s[LOCKED_INTENT], "wifi_drops");
        assert_eq!(s[ACTIVE_CASE_ID], "case-9");
        // Another Focus in the same chat, and another chat: absent.
        let other_focus = read_seeds(&mut conn, Some(CHAT), "other-focus", &names, Utc::now())
            .await
            .unwrap();
        assert!(other_focus.is_empty());
        let other_chat = read_seeds(&mut conn, Some("chat-2"), FOCUS, &names, Utc::now())
            .await
            .unwrap();
        assert!(other_chat.is_empty());
    }

    #[tokio::test]
    async fn derived_names_render_the_case_its_strategies_and_deltas() {
        let mut conn = mem_outputs_conn().await;
        open_case(
            &mut conn,
            &NewCase {
                case_id: "case-1",
                focus_id: FOCUS,
                context_key: CHAT,
                primary_entity_id: "ent-router",
                subject_label: "Living room router",
                symptoms: "drops every {hour}",
                opened_by_run: "run-1",
            },
        )
        .await
        .unwrap();
        append_case_strategy(
            &mut conn,
            CHAT,
            &NewStrategy {
                case_id: "case-1",
                source_run_id: "run-1",
                source_action_id: "step_a",
                source_attempt: 0,
                strategy: "reboot the router",
                outcome: "no change",
                user_signal: Some("still drops"),
                rung_at: None,
            },
        )
        .await
        .unwrap();
        let changed_at = Utc::now().to_rfc3339();
        record_config_delta(
            &mut conn,
            &NewConfigDelta {
                delta_id: "delta-1",
                focus_id: FOCUS,
                source_case_id: None,
                context_key: CHAT,
                session_reference: "sess-1",
                description: "moved the router to channel 36",
                relevance_tags: &[],
                entity_ids: &["ent-router"],
                changed_at: Some(&changed_at),
                recorded_by_run: "run-1",
            },
        )
        .await
        .unwrap();
        put(&mut conn, ACTIVE_CASE_ID, "case-1").await;

        let names = [
            OPEN_TICKET_STATE,
            OPEN_TICKETS_BRIEF,
            CASE_CONTEXT,
            DELTA_CONTEXT,
        ];
        let s = read_seeds(&mut conn, Some(CHAT), FOCUS, &names, Utc::now())
            .await
            .unwrap();
        assert_eq!(s[OPEN_TICKET_STATE], "one");
        // items.id=889: each line starts with the ticket's handle, never its id.
        let ticket = handles::case_handle(FOCUS, "case-1");
        assert_eq!(
            s[OPEN_TICKETS_BRIEF],
            format!("- {ticket}: Living room router (open, step 1 of 5)")
        );
        assert!(!s[OPEN_TICKETS_BRIEF].contains("case-1"));
        let case = &s[CASE_CONTEXT];
        assert!(case.contains(&format!("Ticket: {ticket}")), "{case}");
        assert!(!case.contains("case-1"), "{case}");
        assert!(case.contains("Subject: Living room router"), "{case}");
        // Braces in stored text cannot become tokens.
        assert!(case.contains("Symptoms: drops every (hour)"), "{case}");
        assert!(!case.contains('{') && !case.contains('}'), "{case}");
        assert!(case.contains("- reboot the router -> no change (user said: still drops)"));
        assert!(s[DELTA_CONTEXT].contains("moved the router to channel 36"));
        assert!(s[DELTA_CONTEXT].contains(&handles::delta_handle(FOCUS, "delta-1")));
        assert!(!s[DELTA_CONTEXT].contains("delta-1"));
        assert!(s[DELTA_CONTEXT].contains("believed-active"));
        // active_case_id was read to find the case but not seeded: not asked for.
        assert!(!s.contains_key(ACTIVE_CASE_ID));
    }

    #[tokio::test]
    async fn many_open_cases_is_many_and_the_brief_is_bounded() {
        let mut conn = mem_outputs_conn().await;
        for i in 0..(BRIEF_CASES + 2) {
            open_case(
                &mut conn,
                &NewCase {
                    case_id: &format!("case-{i}"),
                    focus_id: FOCUS,
                    context_key: CHAT,
                    primary_entity_id: &format!("ent-{i}"),
                    subject_label: &format!("device {i}"),
                    symptoms: "",
                    opened_by_run: "run-1",
                },
            )
            .await
            .unwrap();
        }
        let s = read_seeds(
            &mut conn,
            Some(CHAT),
            FOCUS,
            &[OPEN_TICKET_STATE, OPEN_TICKETS_BRIEF],
            Utc::now(),
        )
        .await
        .unwrap();
        assert_eq!(s[OPEN_TICKET_STATE], "many");
        assert_eq!(s[OPEN_TICKETS_BRIEF].lines().count(), BRIEF_CASES + 1);
        assert!(s[OPEN_TICKETS_BRIEF].ends_with("- and 2 more"));
    }

    /// Two Focuses in one persona, each with open cases: the seeds of the running
    /// Focus see only its own cases (open_ticket_state and open_tickets_brief), and
    /// an active_case_id that points at the other Focus's case reads as no case.
    #[tokio::test]
    async fn open_ticket_seeds_see_only_the_running_focuss_cases() {
        let mut conn = mem_outputs_conn().await;
        let open = |id: &'static str, focus: &'static str, label: &'static str| NewCase {
            case_id: id,
            focus_id: focus,
            context_key: CHAT,
            primary_entity_id: "ent-shared",
            subject_label: label,
            symptoms: "",
            opened_by_run: "run-1",
        };
        // tech-support: two open cases. other-focus: one open case.
        for c in [
            open("ts-1", FOCUS, "TS device one"),
            open("ts-2", FOCUS, "TS device two"),
            open("of-1", "other-focus", "OTHER device"),
        ] {
            open_case(&mut conn, &c).await.unwrap();
        }
        let names = [OPEN_TICKET_STATE, OPEN_TICKETS_BRIEF];
        let ts = read_seeds(&mut conn, Some(CHAT), FOCUS, &names, Utc::now())
            .await
            .unwrap();
        assert_eq!(ts[OPEN_TICKET_STATE], "many");
        assert!(ts[OPEN_TICKETS_BRIEF].contains("TS device one"));
        assert!(ts[OPEN_TICKETS_BRIEF].contains("TS device two"));
        assert!(!ts[OPEN_TICKETS_BRIEF].contains("OTHER device"));

        let other = read_seeds(&mut conn, Some(CHAT), "other-focus", &names, Utc::now())
            .await
            .unwrap();
        assert_eq!(other[OPEN_TICKET_STATE], "one");
        assert!(other[OPEN_TICKETS_BRIEF].contains("OTHER device"));
        assert!(!other[OPEN_TICKETS_BRIEF].contains("TS device"));

        // A Focus with no cases of its own sees none, though the persona has three.
        let third = read_seeds(&mut conn, Some(CHAT), "third-focus", &names, Utc::now())
            .await
            .unwrap();
        assert_eq!(third[OPEN_TICKET_STATE], "none");
        assert_eq!(third[OPEN_TICKETS_BRIEF], "");

        // case_context: the other Focus's case is not this Focus's case.
        put(&mut conn, ACTIVE_CASE_ID, "of-1").await;
        let ctx = read_seeds(&mut conn, Some(CHAT), FOCUS, &[CASE_CONTEXT], Utc::now())
            .await
            .unwrap();
        assert_eq!(ctx[CASE_CONTEXT], "");
    }

    /// decisions.id=509: configuration history is not used in another Focus.
    /// Two Focuses record deltas on the same device: delta_context shows the
    /// running Focus's own only.
    #[tokio::test]
    async fn delta_context_sees_only_the_running_focuss_deltas() {
        let mut conn = mem_outputs_conn().await;
        open_case(
            &mut conn,
            &NewCase {
                case_id: "ts-1",
                focus_id: FOCUS,
                context_key: CHAT,
                primary_entity_id: "ent-shared",
                subject_label: "Shared device",
                symptoms: "",
                opened_by_run: "run-1",
            },
        )
        .await
        .unwrap();
        put(&mut conn, ACTIVE_CASE_ID, "ts-1").await;
        let changed_at = Utc::now().to_rfc3339();
        for (id, focus, description) in [
            ("d-mine", FOCUS, "MY FOCUS CHANGE"),
            ("d-theirs", "other-focus", "OTHER FOCUS CHANGE"),
        ] {
            record_config_delta(
                &mut conn,
                &NewConfigDelta {
                    delta_id: id,
                    focus_id: focus,
                    source_case_id: None,
                    context_key: CHAT,
                    session_reference: "sess-1",
                    description,
                    relevance_tags: &[],
                    entity_ids: &["ent-shared"],
                    changed_at: Some(&changed_at),
                    recorded_by_run: "run-1",
                },
            )
            .await
            .unwrap();
        }
        let s = read_seeds(&mut conn, Some(CHAT), FOCUS, &[DELTA_CONTEXT], Utc::now())
            .await
            .unwrap();
        assert!(s[DELTA_CONTEXT].contains("MY FOCUS CHANGE"));
        assert!(!s[DELTA_CONTEXT].contains("OTHER FOCUS CHANGE"));
    }

    // -- fail closed --------------------------------------------------------

    #[tokio::test]
    async fn write_mode_without_a_chat_fails_the_pass() {
        let mut conn = mem_outputs_conn().await;
        let e = read_seeds(&mut conn, None, FOCUS, &[WRITE_MODE], Utc::now())
            .await
            .unwrap_err();
        assert_eq!(e, SeedError::NoContextKey(WRITE_MODE));
        // Other session names with no chat are simply absent.
        let ok = read_seeds(&mut conn, None, FOCUS, &[LOCKED_INTENT], Utc::now())
            .await
            .unwrap();
        assert!(ok.is_empty());
    }

    /// The latch cannot be read (the table is gone): the pass fails. It is not
    /// seeded as open, which is the one thing write_mode must never do.
    #[tokio::test]
    async fn an_unreadable_latch_fails_the_pass_and_never_seeds_open() {
        let mut conn = mem_outputs_conn().await;
        sqlx::query("DROP TABLE session_variables")
            .execute(&mut conn)
            .await
            .unwrap();
        let e = read_seeds(&mut conn, Some(CHAT), FOCUS, &[WRITE_MODE], Utc::now())
            .await
            .unwrap_err();
        assert!(
            matches!(
                e,
                SeedError::Store {
                    name: "write_mode",
                    ..
                }
            ),
            "{e:?}"
        );
        // A session variable read fails the same way.
        let e = read_seeds(&mut conn, Some(CHAT), FOCUS, &[LOCKED_INTENT], Utc::now())
            .await
            .unwrap_err();
        assert!(matches!(
            e,
            SeedError::Store {
                name: "locked_intent",
                ..
            }
        ));
    }

    #[tokio::test]
    async fn an_unreadable_case_store_fails_the_pass() {
        let mut conn = mem_outputs_conn().await;
        sqlx::query("DROP TABLE case_records")
            .execute(&mut conn)
            .await
            .unwrap();
        for name in [OPEN_TICKET_STATE, OPEN_TICKETS_BRIEF] {
            let e = read_seeds(&mut conn, Some(CHAT), FOCUS, &[name], Utc::now())
                .await
                .unwrap_err();
            assert!(matches!(e, SeedError::Store { .. }), "{name}: {e:?}");
        }
        put(&mut conn, ACTIVE_CASE_ID, "case-1").await;
        let e = read_seeds(&mut conn, Some(CHAT), FOCUS, &[CASE_CONTEXT], Utc::now())
            .await
            .unwrap_err();
        assert!(matches!(e, SeedError::Store { .. }), "{e:?}");
    }

    #[tokio::test]
    async fn a_stored_value_that_fails_its_validator_fails_the_pass() {
        let mut conn = mem_outputs_conn().await;
        // The typed writer does not check the shape of an intent, so the seed
        // validator is the line of defence.
        put(&mut conn, LOCKED_INTENT, "Not An Intent").await;
        let e = read_seeds(&mut conn, Some(CHAT), FOCUS, &[LOCKED_INTENT], Utc::now())
            .await
            .unwrap_err();
        assert_eq!(e, SeedError::InvalidValue(LOCKED_INTENT));
        // The error carries no stored value.
        assert!(!e.to_string().contains("Not An Intent"));
    }

    #[test]
    fn cleaning_neutralises_braces_and_caps_on_a_char_boundary() {
        assert_eq!(clean("a {b} c"), "a (b) c");
        let long = "é".repeat(DERIVED_TEXT_MAX_BYTES);
        let capped = cap(long);
        assert!(capped.len() <= DERIVED_TEXT_MAX_BYTES);
        assert!(capped.is_char_boundary(capped.len()));
    }

    // -- items.id=895: the per-pass artifacts -----------------------------------

    async fn put_art(
        conn: &mut SqliteConnection,
        chat: &str,
        focus: &str,
        k: ArtifactKind,
        text: &str,
    ) {
        pass_artifact_store::put_artifact(
            conn,
            &pass_artifact_store::ArtifactWrite {
                context_key: chat,
                focus_id: focus,
                kind: k,
                content: text,
                written_by_run: "run-1",
                expected_version: None,
            },
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn artifact_names_are_absent_until_written_and_have_no_entry() {
        let mut conn = mem_outputs_conn().await;
        let s = read_seeds(
            &mut conn,
            Some(CHAT),
            FOCUS,
            &spec::ARTIFACT_NAMES,
            Utc::now(),
        )
        .await
        .unwrap();
        assert!(
            s.is_empty(),
            "no entry, so a guard is false and a token renders nothing"
        );
    }

    #[tokio::test]
    async fn artifacts_are_seeded_for_this_chat_and_focus_only_and_cleaned() {
        let mut conn = mem_outputs_conn().await;
        put_art(
            &mut conn,
            CHAT,
            FOCUS,
            ArtifactKind::ContentBrief,
            "Brief: {user_input} and {x}",
        )
        .await;
        put_art(
            &mut conn,
            CHAT,
            FOCUS,
            ArtifactKind::DeltaItems,
            "[{\"a\": 1}]",
        )
        .await;
        put_art(
            &mut conn,
            CHAT,
            "other-focus",
            ArtifactKind::VoiceGuidance,
            "OTHER FOCUS",
        )
        .await;
        put_art(
            &mut conn,
            "chat-other",
            FOCUS,
            ArtifactKind::VoiceGuidance,
            "OTHER CHAT",
        )
        .await;

        let s = read_seeds(
            &mut conn,
            Some(CHAT),
            FOCUS,
            &spec::ARTIFACT_NAMES,
            Utc::now(),
        )
        .await
        .unwrap();
        assert_eq!(s.len(), 2, "{s:?}");
        // Braces cannot become tokens in a prompt.
        assert_eq!(s["content_brief"], "Brief: (user_input) and (x)");
        assert_eq!(s["delta_items"], "[(\"a\": 1)]");
        assert!(
            !s.contains_key("voice_guidance"),
            "another Focus and chat are invisible"
        );
        // Only the requested names are read.
        let only = read_seeds(
            &mut conn,
            Some(CHAT),
            FOCUS,
            &[spec::DELTA_ITEMS],
            Utc::now(),
        )
        .await
        .unwrap();
        assert_eq!(only.len(), 1);
        // No chat means nothing to read.
        let none = read_seeds(&mut conn, None, FOCUS, &spec::ARTIFACT_NAMES, Utc::now())
            .await
            .unwrap();
        assert!(none.is_empty());
    }

    #[tokio::test]
    async fn an_unreadable_or_invalid_artifact_fails_the_pass_without_naming_text() {
        let mut conn = mem_outputs_conn().await;
        // The store refuses a blank artifact, so plant one past it, as a corrupt row.
        sqlx::query(
            "INSERT INTO pass_artifacts
             (context_key, focus_id, kind, content, written_by_run, created_at, updated_at)
             VALUES (?, ?, 'content_brief', '   ', 'r', 't', 't')",
        )
        .bind(CHAT)
        .bind(FOCUS)
        .execute(&mut conn)
        .await
        .unwrap();
        let e = read_seeds(
            &mut conn,
            Some(CHAT),
            FOCUS,
            &[spec::CONTENT_BRIEF],
            Utc::now(),
        )
        .await
        .unwrap_err();
        assert_eq!(e, SeedError::InvalidValue(spec::CONTENT_BRIEF));

        let mut conn = mem_outputs_conn().await;
        put_art(
            &mut conn,
            CHAT,
            FOCUS,
            ArtifactKind::ContentBrief,
            "SECRET-BRIEF-TEXT",
        )
        .await;
        sqlx::query("ALTER TABLE pass_artifacts RENAME TO pass_artifacts_gone")
            .execute(&mut conn)
            .await
            .unwrap();
        let e = read_seeds(
            &mut conn,
            Some(CHAT),
            FOCUS,
            &[spec::CONTENT_BRIEF],
            Utc::now(),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(
                e,
                SeedError::Store {
                    name: "content_brief",
                    ..
                }
            ),
            "{e:?}"
        );
        assert!(!format!("{e} {e:?}").contains("SECRET-BRIEF-TEXT"));
    }

    #[test]
    fn an_artifact_name_in_a_template_a_declared_input_or_a_guard_is_a_required_name() {
        let templ = step("t", "Write from {content_brief}");
        let mut declared = step("d", "plain");
        declared.declared_inputs = vec!["voice_guidance".to_owned()];
        let mut guarded = step("g", "plain");
        guarded.guard =
            Guard::from_columns(Some("validation_report"), Some("present"), None).unwrap();
        assert_eq!(
            required_names(&[templ, declared, guarded]),
            [
                spec::CONTENT_BRIEF,
                spec::VOICE_GUIDANCE,
                spec::VALIDATION_REPORT
            ]
        );
        // A pass that names none of them reads none of them.
        assert!(required_names(&[step("x", "{user_input}")]).is_empty());
    }
}
