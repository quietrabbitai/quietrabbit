// src-tauri/src/conductor/store_seed_tests.rs
//
// items.id=886 (decisions.id=939 section 3): the CANARY TEST. Content from the
// session store, the case store and the config-delta store must never reach the
// generic extraction pass's request, however a step came to hold it.
//
// What is real: the persona's encrypted outputs.db (QR_DATA_ROOT scratch dir,
// real migrations), the typed store writers that put the canaries there, the
// production seed_pass() FocusRun::execute() calls at pass start, the production
// render_prompt() with the production StoreTokens filter, the production
// extraction_step_outputs() (taint closure) and extract_candidates(), and a real
// HTTP request to a loopback listener standing in for Ollama
// (ollama_sidecar::ForcedPortGuard). What is stubbed: the model. The "echo"
// step returns its own rendered prompt, which is the worst a model can do, so a
// canary that reached a prompt is in that step's recorded output.
//
// A test that passes because nothing reached the prompt would prove nothing, so
// the test also asserts the canaries ARE in the echoing steps' output and that
// the control step IS in the extraction request. Disabling the taint drop in
// extraction_step_outputs makes it fail (shown at build time, items.id=886).

use std::collections::HashMap;

use crate::conductor::executor::{collect_output_vars, render_prompt, StepContext};
use crate::conductor::lifecycle::{extraction_step_outputs, FocusDefinition};
use crate::conductor::seeding::seed_pass;
use crate::conductor::tokens::{ActionDefinition, ExternalAccess, StepType};
use crate::conductor::types::{TaskStep, TaskTrack};
use crate::persistence::case_store::{
    append_case_strategy, open_case, record_config_delta, NewCase, NewConfigDelta, NewStrategy,
};
use crate::persistence::output_store::open_outputs_db;
use crate::persistence::session_store::{put_session_variable, SessionWrite};
use crate::providers::loopback_test_server::{Canned, LoopbackServer};
use crate::providers::ollama_client::OllamaClient;
use crate::spend::gate::RunAttendance;

const USER: &str = "canary-user";
const PERSONA: &str = "canary-persona";
const KEY: &str = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";
const CHAT: &str = "canary-chat";
const FOCUS: &str = "tech-support";

// One canary per store, plus the extra case-store columns that reach prompts.
const C_SESSION: &str = "canary_intent_zq7";
const C_SUBJECT: &str = "CANARY_SUBJECT_ZQ7";
const C_SYMPTOMS: &str = "CANARY_SYMPTOMS_ZQ7";
const C_STRATEGY: &str = "CANARY_STRATEGY_ZQ7";
const C_DELTA: &str = "CANARY_DELTA_ZQ7";
const CANARIES: [&str; 5] = [C_SESSION, C_SUBJECT, C_SYMPTOMS, C_STRATEGY, C_DELTA];

const CONTROL: &str = "CONTROL_PHRASE_ZQ7";

fn step(id: &str, template: &str, out: &str) -> ActionDefinition {
    ActionDefinition {
        action_id: id.to_owned(),
        display_name: id.to_owned(),
        guide_id: "g".to_owned(),
        task_type: "general".to_owned(),
        routing_tier: ExternalAccess::LocalOnly,
        step_type: StepType::Generate,
        output_var: Some(out.to_owned()),
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

fn focus_def(steps: Vec<ActionDefinition>) -> FocusDefinition {
    FocusDefinition {
        focus_id: FOCUS.to_owned(),
        display_name: "Canary Focus".to_owned(),
        description: String::new(),
        version: "1".to_owned(),
        max_routing_tier: ExternalAccess::LocalOnly,
        steps,
        output_type: "text".to_owned(),
        suggest_in_focuses: Vec::new(),
        multi_source_validation: false,
        generic_title_template: String::new(),
        high_priority_trigger: None,
    }
}

fn ctx(
    step: &ActionDefinition,
    user_input: &str,
    store_tokens: &crate::conductor::taint::StoreTokens,
) -> StepContext {
    StepContext {
        stop: None,
        step: step.clone(),
        focus_id: FOCUS.to_owned(),
        focus_run_id: "canary-run".to_owned(),
        user_input: user_input.to_owned(),
        persona_context: String::new(),
        focus_external_access: ExternalAccess::LocalOnly,
        effective_access: ExternalAccess::LocalOnly,
        execution_tier: 1,
        abstraction_tier: 1,
        raw_abstraction: 1,
        floor_consent_preference: None,
        qr_hosted_provider_preference: None,
        qr_hosted_ceiling_excluded: None,
        next_execution_tier: None,
        retry_count: 0,
        focus_name: "Canary Focus".to_owned(),
        user_id: USER.to_owned(),
        persona_id: PERSONA.to_owned(),
        key_hex: KEY.to_owned(),
        attendance: RunAttendance::Attended,
        stream: None,
        bound: None,
        store_tokens: store_tokens.clone(),
    }
}

/// The stubbed model: every step echoes its own rendered prompt, recorded on
/// the track exactly as execute_step() records a real reply.
fn run_echo_pass(
    def: &FocusDefinition,
    user_input: &str,
    store_tokens: &crate::conductor::taint::StoreTokens,
) -> TaskTrack {
    let mut track = TaskTrack::new();
    for s in &def.steps {
        let prompt = render_prompt(
            &ctx(s, user_input, store_tokens),
            &collect_output_vars(track.steps()),
            track.last_output().unwrap_or(""),
            "",
        );
        track.add_step(TaskStep {
            action_id: s.action_id.clone(),
            output_var: s.output_var.clone(),
            content: format!("ECHO[{prompt}]"),
            sensitivity_severity: 1,
            routing_tier_used: 1,
            personal_flagged: false,
            audit_unavailable: false,
        });
    }
    track
}

fn content_of<'a>(track: &'a TaskTrack, id: &str) -> &'a str {
    &track
        .steps()
        .iter()
        .find(|s| s.action_id == id)
        .unwrap_or_else(|| panic!("step {id} did not run"))
        .content
}

/// Put one canary in each of the three stores through the typed writers.
async fn plant_canaries() {
    let mut conn = open_outputs_db(USER, PERSONA, KEY).await.unwrap();

    // Session store.
    for (name, value) in [
        ("locked_intent", C_SESSION),
        ("active_case_id", "case-canary"),
    ] {
        put_session_variable(
            &mut conn,
            &SessionWrite {
                context_key: CHAT,
                focus_id: FOCUS,
                name,
                value,
                written_by_run: "run-plant",
                expected_version: None,
            },
        )
        .await
        .unwrap();
    }

    // Case store.
    open_case(
        &mut conn,
        &NewCase {
            case_id: "case-canary",
            focus_id: FOCUS,
            context_key: CHAT,
            primary_entity_id: "ent-canary",
            subject_label: C_SUBJECT,
            symptoms: C_SYMPTOMS,
            opened_by_run: "run-plant",
        },
    )
    .await
    .unwrap();
    append_case_strategy(
        &mut conn,
        CHAT,
        &NewStrategy {
            case_id: "case-canary",
            source_run_id: "run-plant",
            source_action_id: "plant",
            source_attempt: 0,
            strategy: C_STRATEGY,
            outcome: "tried",
            user_signal: None,
            rung_at: None,
        },
    )
    .await
    .unwrap();

    // Config-delta store.
    let changed_at = chrono::Utc::now().to_rfc3339();
    record_config_delta(
        &mut conn,
        &NewConfigDelta {
            delta_id: "delta-canary",
            focus_id: FOCUS,
            source_case_id: None,
            context_key: CHAT,
            session_reference: "sess-plant",
            description: C_DELTA,
            relevance_tags: &[],
            entity_ids: &["ent-canary"],
            changed_at: Some(&changed_at),
            recorded_by_run: "run-plant",
        },
    )
    .await
    .unwrap();
}

struct TrustGuard;
impl TrustGuard {
    fn trusted() -> Self {
        crate::ollama_sidecar::force_trust_for_test(true);
        TrustGuard
    }
}
impl Drop for TrustGuard {
    fn drop(&mut self) {
        crate::ollama_sidecar::force_trust_for_test(false);
    }
}

/// Run `f` with QR_DATA_ROOT on a scratch dir, serialised with every other test
/// that touches the environment or the forced Ollama port.
async fn sandbox<F: std::future::Future>(f: F) -> F::Output {
    let _lock = crate::test_support::ENV_MUTEX.lock().await;
    let saved = std::env::var("QR_DATA_ROOT").ok();
    let dir = tempfile::tempdir().expect("tempdir");
    std::env::set_var("QR_DATA_ROOT", dir.path());
    let out = f.await;
    match saved {
        Some(v) => std::env::set_var("QR_DATA_ROOT", v),
        None => std::env::remove_var("QR_DATA_ROOT"),
    }
    out
}

/// The focus under test. Row order is deliberate: the transitive and
/// previous_output steps come after the root, the control last.
fn canary_focus() -> FocusDefinition {
    focus_def(vec![
        // Root: names every store-backed name.
        step(
            "diagnose",
            "Case:\n{case_context}\nChanges:\n{delta_context}\nIntent: {locked_intent}\n\
             Open: {open_tickets_brief}",
            "echo1",
        ),
        // Transitive: names the root's output variable only.
        step("summarize", "Summary of: {echo1}", "echo2"),
        // previous_output after a tainted row, naming no store name itself.
        step("carry", "Carry forward: {previous_output}", "echo3"),
        // Control: untainted. Its user turn tries to pull store values in.
        step("control", "User said: {user_input}", "control_out"),
    ])
}

/// What the typed user turn says: a plain phrase and three placeholders that
/// name store-backed values. None of them may be substituted.
fn hostile_user_turn() -> String {
    format!("{CONTROL} {{case_context}} {{echo1}} {{locked_intent}}")
}

#[tokio::test]
async fn no_canary_reaches_the_extraction_request() {
    sandbox(async {
        plant_canaries().await;
        let def = canary_focus();
        let row_vars: Vec<Option<String>> =
            def.steps.iter().map(|s| s.output_var.clone()).collect();

        // Pass start: the production seeding.
        let tokens = seed_pass(USER, PERSONA, KEY, Some(CHAT), FOCUS, &def.steps, &row_vars)
            .await
            .expect("seeding");
        // The hostile turn is rendered many times (see the note in
        // a_user_turn_cannot_pull_a_store_value_into_an_unnamed_step): every
        // pass must leave the control step clean, and the last one is the track
        // the rest of the test inspects.
        let mut track = run_echo_pass(&def, &hostile_user_turn(), &tokens);
        for _ in 0..63 {
            let control = content_of(&track, "control");
            for c in CANARIES {
                assert!(!control.contains(c), "control leaked {c}: {control}");
            }
            track = run_echo_pass(&def, &hostile_user_turn(), &tokens);
        }

        // The test is not vacuous: every canary reached the root's prompt, and
        // the taint carried to the steps that never named a store value.
        for c in CANARIES {
            assert!(content_of(&track, "diagnose").contains(c), "root lacks {c}");
        }
        assert!(content_of(&track, "summarize").contains(C_SUBJECT));
        assert!(content_of(&track, "carry").contains(C_SUBJECT));
        // The hostile user turn pulled nothing in: the control step rendered
        // its phrase and nothing else.
        let control = content_of(&track, "control");
        assert!(control.contains(CONTROL), "{control}");
        for c in CANARIES {
            assert!(!control.contains(c), "control leaked {c}: {control}");
        }

        // The extraction input, then the real extraction request.
        let outputs = extraction_step_outputs(&track, &def);
        assert_eq!(
            outputs
                .iter()
                .map(|(id, _)| id.as_str())
                .collect::<Vec<_>>(),
            ["control"],
            "only the untainted step is offered to extraction"
        );

        let _trust = TrustGuard::trusted();
        let server = LoopbackServer::start(vec![Canned::json(
            200,
            r#"{"response":"[]","done":true,"model":"llama3.2:3b"}"#,
        )]);
        let port: u16 = server.base().rsplit(':').next().unwrap().parse().unwrap();
        let _port = crate::ollama_sidecar::ForcedPortGuard::set(port);
        let candidates = crate::conductor::extract::extract_candidates(
            &outputs,
            &[],
            &OllamaClient::new(),
            crate::conductor::extract::SOURCE_CONDUCTOR_BATCH,
        )
        .await;
        assert!(candidates.is_empty());

        let seen = server.captured();
        assert_eq!(seen.len(), 1, "exactly one extraction request");
        let body = &seen[0].body;
        assert!(seen[0].path.contains("/api/generate"), "{}", seen[0].path);
        assert!(body.contains(CONTROL), "the control step is in the request");
        for c in CANARIES {
            assert!(
                !body.contains(c),
                "canary {c} reached the extraction request"
            );
        }
        // The store's own text is not there in any other form either.
        for needle in ["case-canary", "ent-canary", "Case:", "Changes:"] {
            assert!(!body.contains(needle), "{needle} reached the request");
        }
    })
    .await;
}

/// The same pass with nothing tainted would send everything: this pins that the
/// extraction input really is the taint closure's doing (and not, say, an empty
/// track), by running a Focus whose root names no bound name.
#[tokio::test]
async fn without_a_bound_name_every_step_is_offered_to_extraction() {
    sandbox(async {
        let def = focus_def(vec![
            step("one", "First {user_input}", "v1"),
            step("two", "Second {v1}", "v2"),
        ]);
        let row_vars: Vec<Option<String>> =
            def.steps.iter().map(|s| s.output_var.clone()).collect();
        let tokens = seed_pass(USER, PERSONA, KEY, None, FOCUS, &def.steps, &row_vars)
            .await
            .expect("no bound name: no store, no chat needed");
        assert!(tokens.seeds.is_empty());
        let track = run_echo_pass(&def, "hello", &tokens);
        assert_eq!(extraction_step_outputs(&track, &def).len(), 2);
    })
    .await;
}

/// A user turn naming a bound token in a step that does NOT name it gets
/// nothing: the filter, not the closure, is what protects an untainted step.
#[tokio::test]
async fn a_user_turn_cannot_pull_a_store_value_into_an_unnamed_step() {
    sandbox(async {
        plant_canaries().await;
        let def = focus_def(vec![
            step("root", "{case_context}", "echo1"),
            step("plain", "Said: {user_input}", "plain_out"),
        ]);
        let row_vars: Vec<Option<String>> =
            def.steps.iter().map(|s| s.output_var.clone()).collect();
        let tokens = seed_pass(USER, PERSONA, KEY, Some(CHAT), FOCUS, &def.steps, &row_vars)
            .await
            .unwrap();
        assert!(tokens.seeds.contains_key("case_context"));
        // render_template() substitutes in HashMap order, which is different on
        // every render, so a leak that needs one particular order shows up
        // within a few dozen passes (each is a fresh map).
        for _ in 0..64 {
            let track = run_echo_pass(&def, "hi {case_context} {echo1}", &tokens);
            assert!(content_of(&track, "root").contains(C_SUBJECT));
            let plain = content_of(&track, "plain");
            assert!(!plain.contains(C_SUBJECT), "{plain}");
            assert_eq!(plain, "ECHO[Said: hi]");
        }
    })
    .await;
}
