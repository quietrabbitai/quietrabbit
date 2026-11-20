// src-tauri/src/conductor/handoff_render.rs
//
// items.id=387 (decisions.id=942 point 7, S3): the OUTBOUND half of a manual
// hand-off, block cb-15, the fourth kind of block (BlockKind::Render).
//
// WHAT IT DOES. It renders, with NO model call, the starter text a person takes to
// a provider by hand, from the row's DECLARED INPUTS only, and records it on the
// TaskTrack as the pass's output (so it is the assistant reply). It then marks the
// hand-off as waiting for its reply: it sets the session name handoff_pending.
// The reply comes back as the next chat turn through the handoff_ingest store
// operation (conductor::handoff_writer).
//
// NO NEW GATE. The starter leaves the device only when the user COPIES it, and the
// copy-time review is the gate (decisions.id=846: the "Copy last response" button
// and every copy path go through request_chat_copy_gate3_review). There is no
// export_chat_starter command. The starter is the pass's last output, because that
// button copies the newest assistant reply: a render row belongs at the END of its
// pass.
//
// WHAT THE LINT DOES WITH IT. A render row makes no call, so it is authored
// LocalOnly and is not a model step, but what it renders is bound for an outside
// provider, so the declared-inputs lint (conductor::declared_inputs) treats it as an
// EXTERNAL step (ActionDefinition::is_external_for_lint):
//   * its template may name only its own declared inputs;
//   * a declared input is an EARLIER row's output variable, or the system token
//     {user_input}. {persona_context}, {voice_profile}, {previous_output} and
//     {focus_context} are refused outright, declared or not (parse below);
//   * it carries no field_requirements (it has none: nothing personal reaches it);
//   * while STORE_NAMES_EXTERNAL_ALLOWED was false it could not name or declare a
//     store-backed name (a bound name, or a variable a tainted step produced).
//     Items.id=884 flipped that flag, so a starter may now carry content_brief, and a
//     Focus that also has a handoff_ingest row may name {user_input} (the pasted
//     reply is in it: conductor::taint). The two DEVICE names are the exception
//     the flag never relaxes (declared_inputs::DEVICE_NAMES_NEVER_EXTERNAL). What a
//     starter built from a store-backed input means for its RECORD is the rule of
//     inherit_from_inputs below.
// It does NOT flip EXTERNAL_MODEL_BLOCKS_ALLOWED: that flag is about model calls
// (hosted_api, and a manual_handoff DESTINATION of a model block, which waits for
// the run plan, items.id=901 and B10). A Focus row cites cb-15 directly.
//
// WHAT IT WRITES. Only handoff_pending (see mark_pending): a hand-off id, no user
// content. It is latch-gated (a one-time chat renders the starter and records
// nothing) and NOT collection-consent gated. Its output is a block row's, so it is
// never read by extraction.
//
// DETERMINISM. The text is the template with each declared input's value put in,
// the values cleaned of braces so one value cannot pull another's token (the same
// cleaning derived seeds get). Nothing is added, trimmed beyond the renderer's own
// trim, or reordered.
//
// THIS MODULE NEVER CALLS A MODEL OR A PROVIDER. It does not use execute_once,
// hosted_step_call or the StepExecutor, and a source scan pins that. Its only use
// of the executor is the token renderer.

use std::collections::{BTreeSet, HashMap};

use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use sqlx::SqliteConnection;

use crate::conductor::bound_spec::{self, HANDOFF_PENDING};
use crate::conductor::executor::{find_tokens, render_template};
use crate::conductor::seeding::clean;
use crate::conductor::tokens::{is_system_token, ActionDefinition, ExternalAccess, StepType};
use crate::conductor::types::TaskTrack;
use crate::persistence::session_store::{self, LatchError, SessionStoreError, SessionWrite};

/// The catalog id of the block.
pub const STABLE_ID: &str = "cb-15";

/// Longest template, in bytes.
pub const TEMPLATE_MAX_BYTES: usize = 8192;
/// Most declared inputs one row may name.
pub const DECLARED_INPUTS_MAX: usize = 32;
const DISPLAY_NAME_MAX_CHARS: usize = 80;
const NAME_MAX: usize = 64;

/// The only system token a starter may name. The others (persona_context,
/// voice_profile, previous_output, focus_context) are refused.
const ALLOWED_SYSTEM_TOKEN: &str = "user_input";

/// The parsed customization of a cb-15 row. deny_unknown_fields at the root, every
/// key required.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StarterConfig {
    pub display_name: String,
    pub output_var: String,
    pub template: String,
    pub declared_inputs: Vec<String>,
}

/// The registry entry of a render block. A render block has no handler and no
/// model-block spec: it parses a configuration and expands to one row.
pub struct RenderBlockSpec {
    pub stable_id: &'static str,
    /// LOAD-time parse and validation of a row's customization JSON.
    pub parse: fn(&Value) -> Result<StarterConfig, String>,
    /// A valid customization, for the admission test.
    pub example: fn() -> Value,
    /// The variable the row records when it runs (guard and lint analysis need it
    /// before the row is built).
    pub output_var: fn(&Value) -> Option<String>,
}

pub static CB15: RenderBlockSpec = RenderBlockSpec {
    stable_id: STABLE_ID,
    parse,
    example,
    output_var,
};

pub fn example() -> Value {
    json!({
        "display_name": "Hand-off starter",
        "output_var": "handoff_starter",
        "template": "Please help me with this request.\n\nRequest: {topic}\n\nReply in plain text.",
        "declared_inputs": ["topic"]
    })
}

pub fn output_var(v: &Value) -> Option<String> {
    parse(v).ok().map(|c| c.output_var)
}

/// A variable or token name: letters, digits and underscores, 1 to 64.
fn valid_name(s: &str) -> bool {
    !s.is_empty() && s.len() <= NAME_MAX && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Parse and validate a cb-15 customization. Each failure has its own message and
/// none carries the template text.
pub fn parse(v: &Value) -> Result<StarterConfig, String> {
    let cfg: StarterConfig = serde_json::from_value(v.clone()).map_err(|e| e.to_string())?;

    let name = cfg.display_name.trim();
    if name.is_empty() || name.chars().count() > DISPLAY_NAME_MAX_CHARS {
        return Err(format!(
            "display_name must be 1 to {DISPLAY_NAME_MAX_CHARS} characters"
        ));
    }
    if name.chars().any(char::is_control) {
        return Err("display_name must not contain control characters".to_owned());
    }

    if !valid_name(&cfg.output_var) {
        return Err("output_var must be letters, digits and underscores, at most 64".to_owned());
    }
    if is_system_token(&cfg.output_var) {
        return Err(format!("output_var '{}' is a system token", cfg.output_var));
    }
    if bound_spec::spec(&cfg.output_var).is_some() {
        return Err(format!(
            "output_var '{}' is a name a pass reads from the store",
            cfg.output_var
        ));
    }

    if cfg.template.trim().is_empty() {
        return Err("template must not be empty".to_owned());
    }
    if cfg.template.len() > TEMPLATE_MAX_BYTES {
        return Err(format!(
            "template is longer than {TEMPLATE_MAX_BYTES} bytes"
        ));
    }
    if cfg.template.contains('\0') {
        return Err("template must not contain a NUL character".to_owned());
    }

    if cfg.declared_inputs.len() > DECLARED_INPUTS_MAX {
        return Err(format!(
            "declared_inputs names more than {DECLARED_INPUTS_MAX} inputs"
        ));
    }
    let mut seen = BTreeSet::new();
    for d in &cfg.declared_inputs {
        if !valid_name(d) {
            return Err(format!(
                "declared input '{d}' is not a usable name (letters, digits and underscores, at most 64)"
            ));
        }
        if !seen.insert(d.as_str()) {
            return Err(format!("declared input '{d}' is named twice"));
        }
        if d == &cfg.output_var {
            return Err(format!(
                "declared input '{d}' is the block's own output variable"
            ));
        }
        refuse_system_token(d)?;
    }

    // Every name the template mentions is declared, and none is a refused token.
    let declared: BTreeSet<&str> = cfg.declared_inputs.iter().map(String::as_str).collect();
    for token in find_tokens(&cfg.template) {
        refuse_system_token(&token)?;
        if !declared.contains(token.as_str()) {
            return Err(format!(
                "the template names '{{{token}}}', which is not in declared_inputs (a starter receives declared inputs only)"
            ));
        }
    }
    Ok(cfg)
}

fn refuse_system_token(name: &str) -> Result<(), String> {
    if is_system_token(name) && name != ALLOWED_SYSTEM_TOKEN {
        return Err(format!(
            "'{name}' is never an input of a hand-off starter (only {{{ALLOWED_SYSTEM_TOKEN}}} among the system tokens)"
        ));
    }
    Ok(())
}

/// The row a cb-15 customization becomes. LocalOnly with no override and no
/// handoff flag, so no tier machinery treats it as a call; the template and the
/// declared inputs ride on it so the lint, the taint closure and the seed
/// requirements see exactly the names the renderer will use. The block id marks it
/// (ActionDefinition::is_render_block).
pub fn expand(
    cfg: &StarterConfig,
    action_id: &str,
    customization: Value,
    guide_id: &str,
) -> ActionDefinition {
    ActionDefinition {
        action_id: action_id.to_owned(),
        display_name: cfg.display_name.trim().to_owned(),
        guide_id: guide_id.to_owned(),
        task_type: "general".to_owned(),
        routing_tier: ExternalAccess::LocalOnly,
        step_type: StepType::default(),
        output_var: Some(cfg.output_var.clone()),
        prompt_template: cfg.template.clone(),
        field_requirements: HashMap::new(),
        options_override: HashMap::new(),
        external_access_override: None,
        requires_user_handoff: false,
        revisitable: false,
        block_stable_id: Some(STABLE_ID.to_owned()),
        block_customization: Some(customization),
        schedule_trigger: None,
        guard: None,
        declared_inputs: cfg.declared_inputs.clone(),
        store_op: None,
        and_guards: Vec::new(),
        requirement: None,
    }
}

/// Why a render could not produce a starter.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum RenderError {
    #[error("the hand-off starter of step '{0}' rendered empty")]
    Empty(String),
}

/// Render the starter of a render row. `values` is the pass's variable view; ONLY
/// the row's declared inputs are taken from it, each cleaned of braces. Pure and
/// deterministic: the same inputs give the same text.
pub fn render_starter(
    step: &ActionDefinition,
    values: &HashMap<String, String>,
    focus_id: &str,
) -> Result<String, RenderError> {
    let mut tokens: HashMap<String, String> = HashMap::new();
    for name in &step.declared_inputs {
        if let Some(v) = values.get(name) {
            tokens.insert(name.clone(), clean(v));
        }
    }
    let text = render_template(&step.prompt_template, &tokens, &step.action_id, focus_id);
    if text.trim().is_empty() {
        return Err(RenderError::Empty(step.action_id.clone()));
    }
    Ok(text)
}

/// What the starter inherits from the steps that produced its declared inputs
/// (items.id=387 S4). The starter carries those steps' content out of the device when
/// the user copies it, so its record must be at least as sensitive as theirs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Inherited {
    /// The highest sensitivity_severity among the producers (1 when none).
    pub sensitivity_severity: i32,
    /// Gate 2 flagged a withheld personal value in any producer.
    pub personal_flagged: bool,
    /// A flagged producer's disclosure-log entry could not be written. Only
    /// meaningful with personal_flagged, as on TaskStep.
    pub audit_unavailable: bool,
}

/// The severity a store-backed declared input counts for: Sensitivity::Personal.
pub fn store_backed_input_severity() -> i32 {
    i32::from(crate::conductor::privacy::types::Sensitivity::Personal.severity())
}

/// The names that are STORE-BACKED in a Focus, for the rule below: every registered
/// bound name (bound_spec::NAMES: a session, case, artifact, hand-off or derived
/// name), every variable a step the taint closure marks as tainted by one produces,
/// and `user_input` in a Focus that has a handoff_ingest row (the pasted reply is in
/// the turn window). `row_vars[i]` is what row `i` records (lifecycle::row_output_vars).
pub fn store_backed_inputs(
    steps: &[ActionDefinition],
    row_vars: &[Option<String>],
) -> std::collections::HashSet<String> {
    let mut set = crate::conductor::seeding::restricted_names(steps, row_vars);
    if crate::conductor::taint::taint_closure(steps, row_vars, bound_spec::NAMES).user_input {
        set.insert("user_input".to_owned());
    }
    set
}

/// The record a render row inherits (items.id=387 S4, and the rider of items.id=884's
/// flip, 2026-11-15).
///
/// RULE. Over each name in the row's `declared_inputs` (only declared inputs count, so
/// a sensitive variable the row does not read does not mark its starter):
///   1. PRODUCERS. If a step produced the value the render uses (the LAST step
///      recorded under that variable, as the variable view resolves it), the starter
///      takes that step's sensitivity_severity (highest wins), its personal_flagged (OR)
///      and its audit_unavailable (OR).
///   2. STORE-BACKED INPUTS COUNT AS PERSONAL. If the name is in `store_backed`
///      (store_backed_inputs: a registered bound name, a variable derived from one, or
///      `user_input` in a Focus that ingests a pasted reply), the starter's
///      sensitivity_severity is at least store_backed_input_severity() (2, personal) and
///      its personal_flagged is true, whether or not a producer exists and whatever the
///      producer recorded. This is a floor: it never lowers rule 1. audit_unavailable
///      comes only from producers (a store read has no disclosure-log entry to lose).
///   3. A declared name that is neither produced nor store-backed (the {user_input}
///      token in a Focus with no hand-off return, a field name) contributes nothing: it
///      has no record to inherit and nothing store-backed in it.
///
/// Why: the starter carries these values out of the device when the user copies it, and
/// before items.id=884 let a store-backed name be declared, an input with no producing
/// step could not be one. Now it can, and a seeded session or case value is the user's
/// own content, so the record says so.
pub fn inherit_from_inputs(
    step: &ActionDefinition,
    track: &TaskTrack,
    store_backed: &std::collections::HashSet<String>,
) -> Inherited {
    let mut out = Inherited {
        sensitivity_severity: 1,
        personal_flagged: false,
        audit_unavailable: false,
    };
    for name in &step.declared_inputs {
        let producer = track
            .steps()
            .iter()
            .rev()
            .find(|s| s.output_var.as_deref() == Some(name.as_str()));
        if let Some(p) = producer {
            out.sensitivity_severity = out.sensitivity_severity.max(p.sensitivity_severity);
            out.personal_flagged |= p.personal_flagged;
            out.audit_unavailable |= p.audit_unavailable;
        }
        if store_backed.contains(name) {
            out.sensitivity_severity = out.sensitivity_severity.max(store_backed_input_severity());
            out.personal_flagged = true;
        }
    }
    out
}

/// The opaque id of a hand-off: `h_` and 12 lowercase hex characters of SHA-256
/// over the run and the row. The same run and row give the same id, so a replay
/// writes the same value.
pub fn handoff_id(run_id: &str, action_id: &str) -> String {
    let digest = Sha256::digest(format!("qr-handoff-v1\0{run_id}\0{action_id}").as_bytes());
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    format!("h_{}", &hex[..12])
}

/// What marking a hand-off pending did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Marked {
    /// handoff_pending now holds this run's id.
    Set,
    /// The chat is a one-time session: nothing was written and the starter is
    /// still shown. (No reply can be taken back in such a chat.)
    Latched,
}

/// Set handoff_pending for this chat and Focus through the typed session store: a
/// compare-and-set over the current version, refused once the chat is latched
/// (reported as Latched, not an error). Not gated by the collection consent: the
/// value holds no user content.
pub async fn mark_pending(
    conn: &mut SqliteConnection,
    context_key: &str,
    focus_id: &str,
    run_id: &str,
    action_id: &str,
) -> Result<Marked, SessionStoreError> {
    let id = handoff_id(run_id, action_id);
    let current =
        session_store::get_session_variable(conn, context_key, focus_id, HANDOFF_PENDING).await?;
    let written = session_store::put_session_variable(
        conn,
        &SessionWrite {
            context_key,
            focus_id,
            name: HANDOFF_PENDING,
            value: &id,
            written_by_run: run_id,
            expected_version: current.map(|c| c.version),
        },
    )
    .await;
    match written {
        Ok(_) => Ok(Marked::Set),
        Err(SessionStoreError::Latch(LatchError::Suppressed)) => Ok(Marked::Latched),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persistence::session_store::test_db::mem_outputs_conn;

    fn ex() -> Value {
        example()
    }

    #[test]
    fn the_example_parses_and_expands_to_one_local_render_row() {
        let cfg = parse(&ex()).unwrap();
        assert_eq!(cfg.output_var, "handoff_starter");
        let step = expand(&cfg, "offer", ex(), "g");
        assert_eq!(step.block_stable_id.as_deref(), Some("cb-15"));
        assert_eq!(step.routing_tier, ExternalAccess::LocalOnly);
        assert!(step.external_access_override.is_none() && !step.requires_user_handoff);
        assert!(step.field_requirements.is_empty() && step.store_op.is_none());
        assert_eq!(step.output_var.as_deref(), Some("handoff_starter"));
        assert_eq!(step.declared_inputs, ["topic"]);
        assert!(step.prompt_template.contains("{topic}"));
        assert!(step.is_render_block() && step.is_external_for_lint());
        assert_eq!(output_var(&ex()).as_deref(), Some("handoff_starter"));
        assert_eq!(output_var(&json!({})), None);
    }

    #[test]
    fn every_key_is_required_and_no_unknown_key_is_accepted() {
        for key in ["display_name", "output_var", "template", "declared_inputs"] {
            let mut v = ex();
            v.as_object_mut().unwrap().remove(key);
            assert!(parse(&v).is_err(), "{key} removed");
        }
        let mut v = ex();
        v["extra"] = json!(1);
        assert!(parse(&v).is_err());
        assert!(parse(&json!([1])).is_err());
        assert!(parse(&json!({})).is_err());
    }

    fn with(key: &str, value: Value) -> Value {
        let mut v = ex();
        v[key] = value;
        v
    }

    #[test]
    fn names_and_sizes_are_checked() {
        for bad in [
            with("display_name", json!("")),
            with("display_name", json!("   ")),
            with("display_name", json!("x".repeat(81))),
            with("display_name", json!("a\u{7}b")),
            with("output_var", json!("")),
            with("output_var", json!("has space")),
            with("output_var", json!("user_input")),
            with("output_var", json!("previous_output")),
            with("output_var", json!("content_brief")),
            with("output_var", json!("handoff_pending")),
            with("output_var", json!("topic")),
            with("template", json!("")),
            with("template", json!("  \n")),
            with("template", json!("a\0b {topic}")),
            with(
                "template",
                json!(format!("{{topic}}{}", "a".repeat(TEMPLATE_MAX_BYTES))),
            ),
            with("declared_inputs", json!(["topic", "topic"])),
            with("declared_inputs", json!(["bad name"])),
            with("declared_inputs", json!([""])),
            with("declared_inputs", json!(["handoff_starter"])),
        ] {
            assert!(parse(&bad).is_err(), "{bad}");
        }
        let many: Vec<String> = (0..=DECLARED_INPUTS_MAX).map(|i| format!("v{i}")).collect();
        assert!(parse(&with("declared_inputs", json!(many))).is_err());
    }

    #[test]
    fn the_template_names_only_declared_inputs_and_only_one_system_token() {
        // An undeclared name.
        let e = parse(&with("template", json!("Hello {other}"))).unwrap_err();
        assert!(e.contains("not in declared_inputs"), "{e}");
        assert!(
            !e.contains("Hello"),
            "the message never carries the template: {e}"
        );
        // The refused system tokens, declared or not.
        for t in [
            "persona_context",
            "voice_profile",
            "previous_output",
            "focus_context",
        ] {
            let mut v = with("template", json!(format!("x {{{t}}}")));
            v["declared_inputs"] = json!(["topic", t]);
            assert!(parse(&v).unwrap_err().contains("never an input"), "{t}");
            // Declared only.
            let mut v = ex();
            v["declared_inputs"] = json!(["topic", t]);
            assert!(
                parse(&v).unwrap_err().contains("never an input"),
                "{t} declared"
            );
        }
        // user_input is the one allowed.
        let mut v = ex();
        v["template"] = json!("Q: {user_input} {topic}");
        v["declared_inputs"] = json!(["topic", "user_input"]);
        assert!(parse(&v).is_ok());
        // A declared input need not appear in the template.
        let mut v = ex();
        v["declared_inputs"] = json!(["topic", "spare"]);
        assert!(parse(&v).is_ok());
        // No tokens at all is fine: a fixed starter.
        let v = json!({"display_name":"n","output_var":"o","template":"Fixed text.",
                       "declared_inputs":[]});
        assert!(parse(&v).is_ok());
    }

    // -- render ------------------------------------------------------------------------

    fn step_of(v: Value) -> ActionDefinition {
        expand(&parse(&v).unwrap(), "offer", v, "g")
    }

    fn vals(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn the_render_takes_declared_inputs_only_and_is_deterministic() {
        let step = step_of(ex());
        let v = vals(&[
            ("topic", "a refund for order 12"),
            // Present in the pass but not declared: never used.
            ("secret_var", "NOT-DECLARED"),
            ("persona_context", "PERSONA-TEXT"),
            ("user_input", "TURN-TEXT"),
        ]);
        let a = render_starter(&step, &v, "f").unwrap();
        assert_eq!(a, render_starter(&step, &v, "f").unwrap());
        assert_eq!(
            a,
            "Please help me with this request.\n\nRequest: a refund for order 12\n\nReply in plain text."
        );
        for leaked in ["NOT-DECLARED", "PERSONA-TEXT", "TURN-TEXT"] {
            assert!(!a.contains(leaked), "{leaked}");
        }
    }

    #[test]
    fn a_value_cannot_pull_another_value_in_through_braces() {
        let v = json!({"display_name":"n","output_var":"o",
                       "template":"A: {a}\nB: {b}","declared_inputs":["a","b"]});
        let step = step_of(v);
        let out = render_starter(
            &step,
            &vals(&[("a", "see {b} and {c}"), ("b", "SECOND")]),
            "f",
        )
        .unwrap();
        assert_eq!(out, "A: see (b) and (c)\nB: SECOND");
        // The same every time, whatever the map's iteration order.
        for _ in 0..20 {
            assert_eq!(
                out,
                render_starter(
                    &step,
                    &vals(&[("a", "see {b} and {c}"), ("b", "SECOND")]),
                    "f"
                )
                .unwrap()
            );
        }
    }

    /// items.id=932: the starter keeps clean() (braces become parentheses), so a value
    /// arrives cleaned and never expands; the engine below it is single-pass as well.
    #[test]
    fn a_value_arrives_cleaned_and_never_expands_in_the_starter() {
        let v = json!({"display_name":"n","output_var":"o",
                       "template":"A: {a}\nU: {user_input}",
                       "declared_inputs":["a","user_input"]});
        let step = step_of(v);
        for _ in 0..32 {
            let out = render_starter(
                &step,
                &vals(&[
                    ("a", "see {user_input} {previous_output}"),
                    ("user_input", "TURN {a}"),
                ]),
                "f",
            )
            .unwrap();
            assert_eq!(out, "A: see (user_input) (previous_output)\nU: TURN (a)");
        }
    }

    #[test]
    fn a_missing_declared_input_is_stripped_and_an_empty_result_is_an_error() {
        let step = step_of(ex());
        let out = render_starter(&step, &vals(&[]), "f").unwrap();
        assert!(!out.contains('{'), "{out}");
        let only = json!({"display_name":"n","output_var":"o","template":"{a}",
                          "declared_inputs":["a"]});
        let step = step_of(only);
        assert_eq!(
            render_starter(&step, &vals(&[]), "f"),
            Err(RenderError::Empty("offer".to_owned()))
        );
        assert_eq!(
            render_starter(&step, &vals(&[("a", "   ")]), "f"),
            Err(RenderError::Empty("offer".to_owned()))
        );
    }

    // -- inheritance of sensitivity and the personal flag ---------------------------

    fn nb() -> std::collections::HashSet<String> {
        std::collections::HashSet::new()
    }

    fn backed(names: &[&str]) -> std::collections::HashSet<String> {
        names.iter().map(|n| (*n).to_owned()).collect()
    }

    fn produced(
        id: &str,
        var: &str,
        sev: i32,
        flagged: bool,
        audit: bool,
    ) -> crate::conductor::types::TaskStep {
        crate::conductor::types::TaskStep {
            action_id: id.to_owned(),
            output_var: Some(var.to_owned()),
            content: format!("{id} content"),
            sensitivity_severity: sev,
            routing_tier_used: 1,
            personal_flagged: flagged,
            audit_unavailable: audit,
        }
    }

    #[test]
    fn the_starter_inherits_the_highest_sensitivity_and_any_personal_flag_of_its_inputs() {
        let cfg = json!({"display_name":"n","output_var":"o","template":"{a} {b}",
                         "declared_inputs":["a","b","c"]});
        let step = step_of(cfg);
        let mut track = TaskTrack::new();
        // No producers yet: nothing to inherit.
        assert_eq!(
            inherit_from_inputs(&step, &track, &nb()),
            Inherited {
                sensitivity_severity: 1,
                personal_flagged: false,
                audit_unavailable: false
            }
        );
        track.add_step(produced("pa", "a", 2, false, false));
        track.add_step(produced("pb", "b", 4, true, true));
        // A sensitive variable the row does not declare never marks the starter.
        track.add_step(produced("px", "undeclared", 4, true, true));
        let got = inherit_from_inputs(&step, &track, &nb());
        assert_eq!(
            got,
            Inherited {
                sensitivity_severity: 4,
                personal_flagged: true,
                audit_unavailable: true
            }
        );
        // The highest wins, and the flag is an OR.
        let only_a = step_of(json!({"display_name":"n","output_var":"o","template":"{a}",
                                   "declared_inputs":["a"]}));
        assert_eq!(
            inherit_from_inputs(&only_a, &track, &nb()),
            Inherited {
                sensitivity_severity: 2,
                personal_flagged: false,
                audit_unavailable: false
            }
        );
    }

    /// THE RIDER (items.id=884 flip): a declared input that is store-backed counts as
    /// personal, producer or not, and the floor never lowers a producer's record.
    #[test]
    fn a_declared_store_backed_input_counts_as_personal() {
        let step = step_of(json!({"display_name":"n","output_var":"o",
                                  "template":"{content_brief} {a}",
                                  "declared_inputs":["content_brief","a"]}));
        let mut track = TaskTrack::new();
        // No producer for content_brief (a seeded name): it still counts.
        let got = inherit_from_inputs(&step, &track, &backed(&["content_brief"]));
        assert_eq!(
            got,
            Inherited {
                sensitivity_severity: 2,
                personal_flagged: true,
                audit_unavailable: false
            },
            "personal severity (2), flagged, no audit loss"
        );
        assert_eq!(store_backed_input_severity(), 2);

        // Not store-backed and not produced: nothing, as before.
        assert_eq!(
            inherit_from_inputs(&step, &track, &nb()),
            Inherited {
                sensitivity_severity: 1,
                personal_flagged: false,
                audit_unavailable: false
            }
        );

        // A producer that recorded MORE keeps its record: the rule is a floor.
        track.add_step(produced("pa", "a", 4, false, true));
        let got = inherit_from_inputs(&step, &track, &backed(&["content_brief", "a"]));
        assert_eq!(
            got,
            Inherited {
                sensitivity_severity: 4,
                personal_flagged: true,
                audit_unavailable: true
            }
        );
        // A store-backed variable whose producer recorded LESS is raised to the floor.
        let mut low = TaskTrack::new();
        low.add_step(produced("pa", "a", 1, false, false));
        let got = inherit_from_inputs(&step, &low, &backed(&["a"]));
        assert_eq!(got.sensitivity_severity, 2);
        assert!(got.personal_flagged);
        // Only DECLARED inputs count: a store-backed name the row does not declare
        // does not mark its starter.
        let only_a = step_of(json!({"display_name":"n","output_var":"o","template":"{a}",
                                   "declared_inputs":["a"]}));
        let got = inherit_from_inputs(&only_a, &nb_track(), &backed(&["content_brief"]));
        assert!(!got.personal_flagged && got.sensitivity_severity == 1);
    }

    fn nb_track() -> TaskTrack {
        TaskTrack::new()
    }

    /// store_backed_inputs: every bound name, every variable a tainted step produces,
    /// and user_input in a Focus that ingests a pasted reply.
    #[test]
    fn the_store_backed_set_is_bound_names_derived_variables_and_a_pasted_user_input() {
        use crate::conductor::tokens::ExternalAccess;
        let local = |id: &str, tpl: &str| {
            let mut s = step_of(json!({"display_name":"n","output_var":"o","template":"t",
                                       "declared_inputs":[]}));
            s.action_id = id.to_owned();
            s.prompt_template = tpl.to_owned();
            s.routing_tier = ExternalAccess::LocalOnly;
            s.block_stable_id = None;
            s
        };
        let steps = vec![
            local("a", "read {content_brief}"),
            local("b", "plain"),
            local("c", "also {derived}"),
        ];
        let vars = vec![
            Some("derived".to_owned()),
            Some("clean".to_owned()),
            Some("second".to_owned()),
        ];
        let set = store_backed_inputs(&steps, &vars);
        assert!(set.contains("content_brief"), "a bound name");
        assert!(
            set.contains("derived"),
            "a variable a tainted step produced"
        );
        assert!(
            set.contains("second"),
            "taint closes over derived variables"
        );
        assert!(
            !set.contains("clean"),
            "an untainted variable is not store-backed"
        );
        assert!(
            !set.contains("user_input"),
            "no hand-off return row in this Focus"
        );
    }

    /// items.id=897 (decisions.id=948 point 2, condition 4): the launch names are
    /// registered bound names, so they are store-backed for the render record. A
    /// starter that declares one counts as personal (severity at least 2 and
    /// personal_flagged) with no producer at all. That is conservative for a launch
    /// answer, which holds no user content, and is accepted.
    #[test]
    fn a_declared_launch_name_is_store_backed_and_counts_as_personal() {
        for name in crate::conductor::bound_spec::LAUNCH_NAMES {
            let step = step_of(json!({"display_name":"n","output_var":"o",
                                      "template": format!("{{{name}}}"),
                                      "declared_inputs":[name]}));
            let mut steps = vec![step.clone()];
            steps[0].action_id = "offer".to_owned();
            let set = store_backed_inputs(&steps, &[Some("o".to_owned())]);
            assert!(set.contains(name), "{name} is store-backed");
            let got = inherit_from_inputs(&step, &TaskTrack::new(), &backed(&[name]));
            assert!(got.sensitivity_severity >= 2, "{name}");
            assert!(got.personal_flagged, "{name}");
            assert!(!got.audit_unavailable, "{name}: only producers set it");
        }
    }

    #[test]
    fn the_last_producer_of_a_variable_is_the_one_inherited_from() {
        let step = step_of(json!({"display_name":"n","output_var":"o","template":"{a}",
                                  "declared_inputs":["a"]}));
        let mut track = TaskTrack::new();
        track.add_step(produced("first", "a", 4, true, false));
        track.add_step(produced("second", "a", 2, false, false));
        // The renderer uses the value of the later step, so it inherits from that one.
        assert_eq!(
            inherit_from_inputs(&step, &track, &nb()),
            Inherited {
                sensitivity_severity: 2,
                personal_flagged: false,
                audit_unavailable: false
            }
        );
    }

    #[test]
    fn a_declared_system_token_or_a_missing_producer_contributes_nothing() {
        let step = step_of(
            json!({"display_name":"n","output_var":"o","template":"{user_input}",
                                  "declared_inputs":["user_input"]}),
        );
        let mut track = TaskTrack::new();
        track.add_step(produced("p", "something", 4, true, true));
        assert_eq!(
            inherit_from_inputs(&step, &track, &nb()),
            Inherited {
                sensitivity_severity: 1,
                personal_flagged: false,
                audit_unavailable: false
            }
        );
    }

    // -- the hand-off id and the pending marker ---------------------------------------

    #[test]
    fn the_handoff_id_is_stable_in_shape_and_valid() {
        let id = handoff_id("run-1", "offer");
        assert_eq!(id, handoff_id("run-1", "offer"));
        assert_ne!(id, handoff_id("run-2", "offer"));
        assert_ne!(id, handoff_id("run-1", "other"));
        assert!(
            bound_spec::spec(HANDOFF_PENDING).is_some_and(|s| (s.validate)(&id)),
            "{id}"
        );
        assert!(!id.contains("run-1"));
    }

    const CHAT: &str = "chat-r";
    const FOCUS: &str = "fx";

    #[tokio::test]
    async fn marking_pending_sets_it_replaces_it_and_is_idempotent() {
        let mut conn = mem_outputs_conn().await;
        assert_eq!(
            mark_pending(&mut conn, CHAT, FOCUS, "r1", "offer")
                .await
                .unwrap(),
            Marked::Set
        );
        let v = session_store::get_session_variable(&mut conn, CHAT, FOCUS, HANDOFF_PENDING)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(v.value, handoff_id("r1", "offer"));
        assert_eq!(v.written_by_run, "r1");
        // The same run again: unchanged.
        mark_pending(&mut conn, CHAT, FOCUS, "r1", "offer")
            .await
            .unwrap();
        // A later run replaces it, with a version bump.
        mark_pending(&mut conn, CHAT, FOCUS, "r2", "offer")
            .await
            .unwrap();
        let v = session_store::get_session_variable(&mut conn, CHAT, FOCUS, HANDOFF_PENDING)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            (v.value.as_str(), v.version),
            (handoff_id("r2", "offer").as_str(), 2)
        );
        // Another Focus or chat is untouched.
        assert!(
            session_store::get_session_variable(&mut conn, CHAT, "other", HANDOFF_PENDING)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn a_latched_chat_records_nothing_and_is_not_an_error() {
        let mut conn = mem_outputs_conn().await;
        session_store::latch_write_suppressed(&mut conn, CHAT, FOCUS, "r0")
            .await
            .unwrap();
        assert_eq!(
            mark_pending(&mut conn, CHAT, FOCUS, "r1", "offer")
                .await
                .unwrap(),
            Marked::Latched
        );
        assert!(
            session_store::get_session_variable(&mut conn, CHAT, FOCUS, HANDOFF_PENDING)
                .await
                .unwrap()
                .is_none()
        );
    }

    // -- the render never reaches a model ----------------------------------------------

    /// THE PIN: nothing in this module (production code) calls a model or a
    /// provider, the executor beyond its token renderer, or the one-send path.
    #[test]
    fn this_module_never_calls_execute_once_or_hosted_step_call() {
        let src = include_str!("handoff_render.rs");
        let body = src.split("#[cfg(test)]").next().unwrap();
        let code: String = body
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        for banned in [
            "execute_once",
            "hosted_step_call",
            "StepExecutor",
            "execute_step",
            "OllamaClient",
            "HostedRequest",
            "render_prompt",
            "ollama",
            "gate3",
            "step_review",
        ] {
            assert!(!code.contains(banned), "handoff_render.rs names {banned}");
        }
        // Its only imports from the executor.
        let executor_uses: Vec<&str> = code
            .lines()
            .filter(|l| l.contains("conductor::executor"))
            .collect();
        assert_eq!(executor_uses.len(), 1, "{executor_uses:?}");
        assert!(executor_uses[0].contains("find_tokens, render_template"));
    }
}
