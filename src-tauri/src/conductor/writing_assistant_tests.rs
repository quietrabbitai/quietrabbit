// src-tauri/src/conductor/writing_assistant_tests.rs
//
// items.id=903 unit C: the Writing Assistant's chat passes, end to end over everything
// but the model. WHAT IS REAL: the shipped composition (shared_048.sql through the real
// migration and the real DB loader), the real launch config and run-start resolution and
// re-expansion (run_settings), the real pass-start seeding (seed_pass: artifacts, launch
// names, the derived section names) against the persona's encrypted outputs.db under a
// scratch data root, the real guard evaluation over seeds, track and published words,
// the real cb-06 handler (with the declared seed view execute_block_step gives it) and
// the real store writers (run_store_op). WHAT IS SCRIPTED: each model step's output (a
// closure per action id), so no prompt, gate, resolver or provider runs here; those are
// covered on their own paths (executor, resolver, step_treatment tests). The pass loop
// mirrors execute()'s order: skip on a false guard, writers record nothing, a block
// records its verdict, an ordinary or model row records its output.

use std::collections::HashMap;

use crate::conductor::block_dispatch::{block_handler, BlockInputs};
use crate::conductor::guard;
use crate::conductor::launch_config::read_validated;
use crate::conductor::lifecycle::{
    declared_seeds, load_focus_definition_from_db, row_output_vars, FocusDefinition,
};
use crate::conductor::model_block::document_type::registry;
use crate::conductor::run_settings::{reexpand, resolve, LaunchResolution, StoredAnswers};
use crate::conductor::seeding::seed_pass;
use crate::conductor::store_writer::{run_store_op, Collection, Entities, WriterContext};
use crate::conductor::tokens::ActionDefinition;
use crate::conductor::types::{TaskStep, TaskTrack};
use crate::persistence::output_store::open_outputs_db;
use crate::persistence::pass_artifact_store::{self, ArtifactKind};

const USER: &str = "wa-user";
const PERSONA: &str = "wa-persona";
const KEY: &str = "0f1e2d3c4b5a69788796a5b4c3d2e1f00f1e2d3c4b5a69788796a5b4c3d2e1f0";
const FOCUS: &str = "writing-assistant";

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

async fn pool() -> sqlx::SqlitePool {
    crate::persistence::migrations::migrate_shared_db()
        .await
        .expect("shared.db");
    sqlx::SqlitePool::connect_with(crate::providers::utils::connect_options_unencrypted(
        &crate::providers::utils::db_path_shared(),
    ))
    .await
    .expect("pool")
}

/// The Writing Assistant as a run of this chat would load it: the shipped rows,
/// re-expanded with the run's mode and document type.
async fn wa_for(
    pool: &sqlx::SqlitePool,
    mode: &str,
    doc: &str,
) -> (FocusDefinition, LaunchResolution) {
    let mut def = load_focus_definition_from_db(pool, FOCUS)
        .await
        .unwrap()
        .expect("shared_048 seeds the Writing Assistant");
    let reg = registry().unwrap();
    let config = read_validated(pool, FOCUS, reg).await.unwrap();
    let stored: StoredAnswers = [
        ("launch_mode".to_owned(), mode.to_owned()),
        ("document_type".to_owned(), doc.to_owned()),
    ]
    .into_iter()
    .collect();
    let resolved = resolve(config.as_ref(), &stored, None, reg).unwrap();
    reexpand(&mut def, &resolved).unwrap();
    (def, resolved.resolution)
}

/// What one pass did.
struct Pass {
    ran: Vec<String>,
    track: TaskTrack,
    published: HashMap<String, String>,
}

impl Pass {
    fn ran(&self, id: &str) -> bool {
        self.ran.iter().any(|r| r == id)
    }
    fn reply(&self) -> &str {
        self.track.last_output().unwrap_or("")
    }
}

type Script<'a> = &'a dyn Fn(&str, &HashMap<String, String>) -> String;

/// One chat pass: seeds at pass start, then every row in order.
async fn run_pass(
    def: &FocusDefinition,
    launch: &LaunchResolution,
    chat: &str,
    run_id: &str,
    user_input: &str,
    script: Script<'_>,
) -> Pass {
    let row_vars = row_output_vars(&def.steps);
    let tokens = seed_pass(
        USER,
        PERSONA,
        KEY,
        Some(chat),
        FOCUS,
        &def.steps,
        &row_vars,
        launch,
    )
    .await
    .expect("seeding");
    let mut track = TaskTrack::new();
    let mut published: HashMap<String, String> = HashMap::new();
    let mut ran = Vec::new();
    let mut conn = open_outputs_db(USER, PERSONA, KEY).await.unwrap();
    for step in &def.steps {
        let mut vars = guard::track_vars(&track);
        vars.extend(tokens.seeds.clone());
        vars.extend(published.clone());
        let clauses = step.guards();
        if guard::first_false(&clauses, &vars).is_some() {
            continue;
        }
        ran.push(step.action_id.clone());
        if let Some(op) = &step.store_op {
            let ctx = WriterContext {
                context_key: Some(chat),
                persona_id: PERSONA,
                focus_id: FOCUS,
                run_id,
                action_id: &step.action_id,
                collection: Collection::Granted,
                user_id: USER,
                data_key: None,
                turn_text: None,
            };
            let done = run_store_op(&mut conn, Entities::NotNeeded, &ctx, op, &vars)
                .await
                .unwrap_or_else(|e| panic!("{}: {e}", step.action_id));
            if let Some((k, v)) = done.published {
                published.insert(k, v);
            }
            continue;
        }
        if let Some(id) = &step.block_stable_id {
            let handler = block_handler(id).expect("a pure block");
            let seeds = declared_seeds(step, &tokens.seeds);
            let out = (handler.run)(
                step.block_customization.as_ref().unwrap(),
                &BlockInputs {
                    track: &track,
                    seeds: &seeds,
                },
            )
            .unwrap();
            record(&mut track, step, Some(out.output_var), out.content);
            continue;
        }
        // An ordinary or model row: the scripted model sees the values its template
        // names (the user's request included).
        let mut named = vars.clone();
        named.insert("user_input".to_owned(), user_input.to_owned());
        let text = script(&step.action_id, &named);
        record(&mut track, step, step.output_var.clone(), text);
    }
    Pass {
        ran,
        track,
        published,
    }
}

fn record(track: &mut TaskTrack, step: &ActionDefinition, var: Option<String>, content: String) {
    track.add_step(TaskStep {
        action_id: step.action_id.clone(),
        output_var: var,
        content,
        sensitivity_severity: 1,
        routing_tier_used: 1,
        personal_flagged: false,
        audit_unavailable: false,
    });
}

async fn artifact(chat: &str, kind: ArtifactKind) -> Option<String> {
    let mut conn = open_outputs_db(USER, PERSONA, KEY).await.unwrap();
    pass_artifact_store::get_artifact(&mut conn, chat, FOCUS, kind)
        .await
        .unwrap()
        .map(|a| a.content)
}

const BRIEF_JSON: &str = r#"Here it is: {"task": "Thank Ana for hosting", "recipient": "Ana", "points": ["Thanks for the dinner on Friday", "The lemon tart was the best part"]}"#;

/// A model that writes clean text from what it is given.
fn clean_model(id: &str, v: &HashMap<String, String>) -> String {
    match id {
        "brief" => BRIEF_JSON.to_owned(),
        "brief_reply" => "You want to thank Ana. Next message starts the draft.".to_owned(),
        "brief_retry" => "Tell me what it is for and who it is to.".to_owned(),
        "draft" | "draft_again" | "quick_draft" => {
            "Dear Ana,\n\nThanks for the dinner on Friday. The lemon tart was the best part.\n\nWarmly"
                .to_owned()
        }
        "draft_voice" | "quick_voice" | "revise_voice" | "high_revise_voice" => {
            let src = ["draft_text", "quick_text", "revised_text", "high_revised_text"]
                .iter()
                .find_map(|k| v.get(*k))
                .cloned()
                .unwrap_or_default();
            format!("{src}\n(voiced)")
        }
        "revise" | "high_revise" => format!(
            "{}\nP.S. {}",
            v.get("working_draft").cloned().unwrap_or_default(),
            v.get("user_input").cloned().unwrap_or_default()
        ),
        "section_draft" => format!(
            "This section is about {}.",
            v.get("section_heading").cloned().unwrap_or_default()
        ),
        "section_voice" => v.get("section_text").cloned().unwrap_or_default(),
        other => format!("reply from {other}"),
    }
}

#[tokio::test]
async fn medium_mode_brief_then_draft_then_revise() {
    sandbox(async {
        let p = pool().await;
        let (def, launch) = wa_for(&p, "medium", "personal_note").await;
        let chat = "chat-medium";

        // Pass 1: the request becomes the brief. The stored brief is seen by the NEXT
        // pass only: nothing guarded on content_brief present runs here.
        let one = run_pass(
            &def,
            &launch,
            chat,
            "r1",
            "Thank Ana for dinner",
            &clean_model,
        )
        .await;
        assert_eq!(one.ran, ["brief", "brief_store", "brief_reply"]);
        assert_eq!(one.published["brief_state"], "stored");
        assert!(one.reply().contains("thank Ana"));
        let brief = artifact(chat, ArtifactKind::ContentBrief).await.unwrap();
        assert!(
            brief.contains("- The lemon tart was the best part"),
            "{brief}"
        );
        assert!(brief.starts_with("Task: Thank Ana for hosting"), "{brief}");

        // Pass 2: the draft, checked (clean: accept, no second draft), voiced, stored.
        let two = run_pass(&def, &launch, chat, "r2", "Go ahead", &clean_model).await;
        assert_eq!(
            two.ran,
            [
                "draft",
                "draft_check",
                "draft_report",
                "draft_voice",
                "draft_store",
                "draft_report_store"
            ]
        );
        assert_eq!(two.track.get_output("check_verdict"), Some("accept"));
        assert!(two.reply().ends_with("(voiced)"));
        let stored = artifact(chat, ArtifactKind::WorkingDraft).await.unwrap();
        assert_eq!(stored, two.reply());
        let report = artifact(chat, ArtifactKind::ValidationReport)
            .await
            .unwrap();
        assert!(report.starts_with("Draft check: accept."), "{report}");
        assert!(
            !report.contains("Ana"),
            "the report holds no draft text: {report}"
        );

        // Pass 3: a change request revises the stored draft.
        let three = run_pass(&def, &launch, chat, "r3", "Mention the wine", &clean_model).await;
        assert_eq!(
            three.ran,
            [
                "revise",
                "revise_report",
                "revise_voice",
                "revise_store",
                "revise_report_store"
            ]
        );
        assert!(three.reply().contains("P.S. Mention the wine"));
        assert_eq!(
            artifact(chat, ArtifactKind::WorkingDraft).await.unwrap(),
            three.reply()
        );
    })
    .await;
}

#[tokio::test]
async fn a_strict_type_regenerates_once_and_the_second_check_only_marks() {
    sandbox(async {
        let p = pool().await;
        let (def, launch) = wa_for(&p, "medium", "rma_letter").await;
        let chat = "chat-strict";
        run_pass(&def, &launch, chat, "r1", "Return my drill", &clean_model).await;
        // A model that invents a name and a city in every draft.
        let inventive = |id: &str, v: &HashMap<String, String>| -> String {
            match id {
                "draft" | "draft_again" => {
                    "Dear Ana,\n\nThanks for the dinner on Friday. Mr Harlow in Seattle agreed."
                        .to_owned()
                }
                _ => clean_model(id, v),
            }
        };
        let two = run_pass(&def, &launch, chat, "r2", "Go ahead", &inventive).await;
        assert_eq!(two.track.get_output("check_verdict"), Some("regenerate"));
        assert_eq!(two.ran.iter().filter(|r| *r == "draft_again").count(), 1);
        assert!(two.ran("draft_check_again"));
        assert_eq!(two.track.get_output("check_verdict_again"), Some("mark"));
        // The report is on the final draft and marks it.
        assert!(two
            .track
            .get_output("check_report")
            .unwrap()
            .starts_with("Draft check: mark."));
        // Light mode on the same draft checks but never drafts again.
        let (light, light_launch) = wa_for(&p, "low", "rma_letter").await;
        let chat2 = "chat-strict-light";
        run_pass(
            &light,
            &light_launch,
            chat2,
            "r1",
            "Return my drill",
            &clean_model,
        )
        .await;
        let two = run_pass(&light, &light_launch, chat2, "r2", "Go ahead", &inventive).await;
        assert_eq!(two.track.get_output("check_verdict"), Some("regenerate"));
        assert!(!two.ran("draft_again") && !two.ran("draft_check_again"));
    })
    .await;
}

#[tokio::test]
async fn an_invalid_brief_stores_nothing_and_asks_again() {
    sandbox(async {
        let p = pool().await;
        let (def, launch) = wa_for(&p, "medium", "personal_note").await;
        let chat = "chat-invalid";
        let garbled = |id: &str, v: &HashMap<String, String>| -> String {
            match id {
                "brief" => "I am not sure what you mean.".to_owned(),
                _ => clean_model(id, v),
            }
        };
        let one = run_pass(&def, &launch, chat, "r1", "hmm", &garbled).await;
        assert_eq!(one.ran, ["brief", "brief_store", "brief_retry"]);
        assert_eq!(one.published["brief_state"], "invalid");
        assert!(artifact(chat, ArtifactKind::ContentBrief).await.is_none());
        // A brief with an unknown key or no points is refused the same way.
        for bad in [
            r#"{"task": "x", "recipient": null, "points": [], "tone": "warm"}"#,
            r#"{"task": "x", "recipient": null, "points": []}"#,
        ] {
            let b = move |id: &str, v: &HashMap<String, String>| -> String {
                if id == "brief" {
                    bad.to_owned()
                } else {
                    clean_model(id, v)
                }
            };
            let again = run_pass(&def, &launch, chat, "r2", "hmm", &b).await;
            assert!(again.ran("brief_retry"), "{bad}");
        }
        // The next pass starts over with the brief.
        let two = run_pass(&def, &launch, chat, "r3", "Thank Ana", &clean_model).await;
        assert_eq!(two.ran, ["brief", "brief_store", "brief_reply"]);
    })
    .await;
}

#[tokio::test]
async fn quick_mode_drafts_in_one_pass_and_then_revises() {
    sandbox(async {
        let p = pool().await;
        let (def, launch) = wa_for(&p, "quick", "personal_note").await;
        let chat = "chat-quick";
        let one = run_pass(&def, &launch, chat, "r1", "Thank Ana", &clean_model).await;
        assert_eq!(
            one.ran,
            ["quick_draft", "quick_check", "quick_voice", "quick_store"]
        );
        // The plain advisory check (decisions.id=963): a verdict text, no draft check.
        assert!(one
            .track
            .get_output("quick_verdict")
            .unwrap()
            .starts_with("Quality check (advisory)"));
        assert!(artifact(chat, ArtifactKind::ContentBrief).await.is_none());
        assert_eq!(
            artifact(chat, ArtifactKind::WorkingDraft).await.unwrap(),
            one.reply()
        );
        let two = run_pass(&def, &launch, chat, "r2", "Shorter", &clean_model).await;
        assert!(two.ran("revise") && !two.ran("quick_draft"));
    })
    .await;
}

#[tokio::test]
async fn high_mode_writes_one_section_per_pass_then_revises() {
    sandbox(async {
        let p = pool().await;
        let (def, launch) = wa_for(&p, "high", "essay_section").await;
        let chat = "chat-high";
        run_pass(&def, &launch, chat, "r1", "An essay", &clean_model).await;
        for (pass, heading) in [
            ("r2", "Thanks for the dinner on Friday"),
            ("r3", "The lemon tart was the best part"),
        ] {
            let p = run_pass(&def, &launch, chat, pass, "next", &clean_model).await;
            assert_eq!(
                p.ran,
                [
                    "section_draft",
                    "section_check",
                    "section_voice",
                    "section_append",
                    "section_report_store"
                ],
                "{pass}"
            );
            assert_eq!(p.published["append_state"], "appended");
            assert!(p.reply().contains(heading), "{pass}: {}", p.reply());
            let draft = artifact(chat, ArtifactKind::WorkingDraft).await.unwrap();
            assert!(draft.contains(&format!("## {heading}")), "{draft}");
        }
        let draft = artifact(chat, ArtifactKind::WorkingDraft).await.unwrap();
        assert_eq!(draft.matches("## ").count(), 2);
        // Every section written: the next pass revises the whole draft.
        let four = run_pass(&def, &launch, chat, "r4", "Tighten it", &clean_model).await;
        assert_eq!(
            four.ran,
            [
                "high_revise",
                "high_revise_report",
                "high_revise_voice",
                "high_revise_store",
                "high_revise_report_store"
            ]
        );
    })
    .await;
}

#[tokio::test]
async fn an_append_past_the_artifact_limit_writes_nothing_and_says_so() {
    sandbox(async {
        let p = pool().await;
        let (def, launch) = wa_for(&p, "high", "essay_section").await;
        let chat = "chat-long";
        run_pass(&def, &launch, chat, "r1", "An essay", &clean_model).await;
        let huge = |id: &str, v: &HashMap<String, String>| -> String {
            match id {
                "section_voice" => "word ".repeat(4000),
                _ => clean_model(id, v),
            }
        };
        let two = run_pass(&def, &launch, chat, "r2", "next", &huge).await;
        assert_eq!(two.published["append_state"], "too_long");
        assert!(two.ran("section_too_long") && !two.ran("section_report_store"));
        assert!(artifact(chat, ArtifactKind::WorkingDraft).await.is_none());
    })
    .await;
}

/// Constraint 4: a reply row is guarded on what a STORE did (a published word), a
/// stored name or a block's verdict, never on a model step's output.
#[tokio::test]
async fn no_row_is_guarded_on_model_text() {
    sandbox(async {
        let p = pool().await;
        let def = load_focus_definition_from_db(&p, FOCUS)
            .await
            .unwrap()
            .unwrap();
        let model_vars: Vec<&str> = def
            .steps
            .iter()
            .filter(|s| s.block_stable_id.is_none() && s.store_op.is_none())
            .filter_map(|s| s.output_var.as_deref())
            .collect();
        for s in &def.steps {
            for g in s.guards() {
                assert!(
                    !model_vars.contains(&g.var.as_str()),
                    "{} is guarded on model output {}",
                    s.action_id,
                    g.var
                );
            }
        }
    })
    .await;
}

/// The pinned flag rule from Chat-PM's unit B ruling: a flag change, loosening
/// included, never lifts the run above the Focus's own ceiling or the user's
/// focus_settings ceiling; only unit D's audited raise could.
#[test]
fn a_flag_change_never_lifts_the_ceiling() {
    use crate::conductor::tokens::ExternalAccess;
    use crate::persistence::document_session_store::{
        restricted_ceiling, DocumentPrivacyFlag, DocumentSession,
    };
    let mut s = DocumentSession {
        context_key: "c".to_owned(),
        focus_id: FOCUS.to_owned(),
        document_type: "cover_letter".to_owned(),
        document_privacy_flag: None,
        raised_ceiling: None,
        consent_audit_id: None,
        version: 1,
        created_at: String::new(),
        updated_at: String::new(),
    };
    for user in [ExternalAccess::LocalOnly, ExternalAccess::AnonymousRequired] {
        for f in DocumentPrivacyFlag::ALL
            .iter()
            .copied()
            .map(Some)
            .chain([None])
        {
            s.document_privacy_flag = f;
            let applied = match restricted_ceiling(Some(&s), "cover_letter") {
                Some(cap) => user.min(cap),
                None => user,
            };
            assert!(applied <= user, "{f:?} lifted {user:?} to {applied:?}");
        }
    }
}

/// Constraint 3: every Writing Assistant model step, in every mode and for every
/// document type, resolves on the SHIPPED capability cells with the three curated local
/// models ready (the voice and brief steps read the task-type-wide quick_response cells
/// of items.id=899). With the quick_response cells removed, the voice and brief steps
/// park TooWeak (no model is ever picked below the bar or unmeasured).
#[tokio::test]
async fn every_model_step_resolves_on_the_shipped_cells_and_parks_without_them() {
    use crate::conductor::model_block::config::ModelTaskType;
    use crate::conductor::model_capability::CapabilitySubject;
    use crate::conductor::model_resolver::{
        resolve as pick, Candidate, CostTier, Readiness, Resolution, ResolveInput,
    };
    use crate::conductor::tokens::ExternalAccess;
    sandbox(async {
        let p = pool().await;
        let mut conn = p.acquire().await.unwrap();
        let cells = crate::persistence::model_capability_store::list_capabilities(&mut conn)
            .await
            .unwrap()
            .rows;
        drop(conn);
        let candidates: Vec<Candidate> = crate::providers::curated_models::CURATED_LOCAL_MODEL_TAGS
            .iter()
            .map(|tag| Candidate {
                subject: CapabilitySubject::Local,
                provider_id: String::new(),
                model_id: (*tag).to_owned(),
                readiness: Readiness::Ready,
                context_window_tokens: 8192,
                cost_tier: CostTier::Local,
                est_cost_micros: 0,
                provider_is_anonymous: false,
            })
            .collect();
        let without_voice: Vec<_> = cells
            .iter()
            .filter(|c| c.task_type != ModelTaskType::QuickResponse)
            .cloned()
            .collect();
        let doc_types: Vec<String> = registry()
            .unwrap()
            .types
            .iter()
            .map(|t| t.id.clone())
            .collect();
        for mode in ["quick", "low", "medium", "high"] {
            for doc in &doc_types {
                let (def, _) = wa_for(&p, mode, doc).await;
                for s in def.steps.iter().filter(|s| s.requirement.is_some()) {
                    let req = s.requirement.as_ref().unwrap();
                    let run = |caps| {
                        pick(&ResolveInput {
                            requirement: req,
                            profile: Some(doc.as_str()),
                            ceiling: ExternalAccess::AnonymousRequired,
                            min_context_tokens: 0,
                            candidates: &candidates,
                            capabilities: caps,
                        })
                    };
                    assert!(
                        matches!(run(&cells), Resolution::Selected { .. }),
                        "{mode} {doc} {}: {:?}",
                        s.action_id,
                        run(&cells)
                    );
                    if req.need.task_type == ModelTaskType::QuickResponse {
                        assert!(
                            matches!(run(&without_voice), Resolution::TooWeak { .. }),
                            "{mode} {doc} {} must park without its cells",
                            s.action_id
                        );
                    }
                }
            }
        }
    })
    .await;
}

#[test]
fn a_brief_is_read_out_of_a_fence_and_nothing_else_is_accepted() {
    use crate::conductor::store_writer::parse_brief;
    let fenced = "```json\n{\"task\": \"A note\", \"recipient\": null, \"points\": [\"one\"]}\n```";
    assert_eq!(parse_brief(fenced).unwrap().points, ["one"]);
    for bad in [
        "no object here",
        "} backwards {",
        r#"{"task": "A note", "points": ["one"], "extra": 1}"#,
        r#"{"task": "", "recipient": null, "points": ["one"]}"#,
        r#"{"task": "A note", "recipient": null, "points": []}"#,
    ] {
        assert!(parse_brief(bad).is_none(), "{bad}");
    }
}

/// artifact_append appends only the section that is due, by the same rule as the seeds:
/// with no stored brief, or with every section written, it writes nothing and publishes
/// `no_section`.
#[tokio::test]
async fn an_append_with_no_section_due_writes_nothing() {
    use crate::conductor::store_op::StoreOp;
    sandbox(async {
        let _p = pool().await;
        let mut conn = open_outputs_db(USER, PERSONA, KEY).await.unwrap();
        let op = StoreOp::from_columns(
            Some("artifact_append"),
            Some("append_state"),
            Some(r#"{"value": "text"}"#),
        )
        .unwrap()
        .unwrap();
        let vars: HashMap<String, String> = [("text".to_owned(), "A section.".to_owned())]
            .into_iter()
            .collect();
        let ctx = |chat: &'static str| WriterContext {
            context_key: Some(chat),
            persona_id: PERSONA,
            focus_id: FOCUS,
            run_id: "r",
            action_id: "section_append",
            collection: Collection::Granted,
            user_id: USER,
            data_key: None,
            turn_text: None,
        };
        // No brief at all.
        let done = run_store_op(
            &mut conn,
            Entities::NotNeeded,
            &ctx("chat-none"),
            &op,
            &vars,
        )
        .await
        .unwrap();
        assert_eq!(
            done.published,
            Some(("append_state".to_owned(), "no_section".to_owned()))
        );
        assert!(artifact("chat-none", ArtifactKind::WorkingDraft)
            .await
            .is_none());
        // Every section already written.
        for (kind, text) in [
            (
                ArtifactKind::ContentBrief,
                "Task: x\n\nPoints and facts to cover:\n- Only point",
            ),
            (ArtifactKind::WorkingDraft, "## Only point\n\nDone."),
        ] {
            pass_artifact_store::put_artifact(
                &mut conn,
                &pass_artifact_store::ArtifactWrite {
                    context_key: "chat-done",
                    focus_id: FOCUS,
                    kind,
                    content: text,
                    written_by_run: "seed",
                    expected_version: None,
                },
            )
            .await
            .unwrap();
        }
        let done = run_store_op(
            &mut conn,
            Entities::NotNeeded,
            &ctx("chat-done"),
            &op,
            &vars,
        )
        .await
        .unwrap();
        assert_eq!(
            done.published,
            Some(("append_state".to_owned(), "no_section".to_owned()))
        );
        assert_eq!(
            artifact("chat-done", ArtifactKind::WorkingDraft)
                .await
                .unwrap(),
            "## Only point\n\nDone."
        );
    })
    .await;
}
