// src-tauri/src/conductor/block_guard_load_tests.rs
//
// items.id=903 unit A (constraint 1, decisions.id=963 rulings 1 and 3), on the DB
// loader: a model block emits its own `present` clause on a required Optional bound
// name, the loader merges it AHEAD of the row's clauses (no row can drop it, a row
// that states it loads, the cap counts it), the run-start re-expansion keeps it, and
// a cb-06 draft-check row declares what it reads (lint-checked, seeded) and gets the
// run's settings at run start.

use std::collections::{BTreeMap, HashMap};

use serde_json::{json, Value};

use super::*;
use crate::conductor::guard::{Condition, Guard};
use crate::conductor::model_block::document_type::registry;
use crate::conductor::run_settings::{reexpand, LaunchResolution, Resolved};

async fn with_root<F: std::future::Future>(f: F) -> F::Output {
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
        .expect("shared.db migrates");
    sqlx::SqlitePool::connect_with(crate::providers::utils::connect_options_unencrypted(
        &crate::providers::utils::db_path_shared(),
    ))
    .await
    .expect("pool")
}

struct Row {
    id: &'static str,
    block: Option<&'static str>,
    guard: Option<(&'static str, &'static str)>,
    clauses: Option<Value>,
    custom: Value,
}

fn row(id: &'static str, block: Option<&'static str>, custom: Value) -> Row {
    Row {
        id,
        block,
        guard: None,
        clauses: None,
        custom,
    }
}

/// A Focus with a fixed document_type question (essay_section, no sender or context
/// fields) and the given rows.
async fn seed(pool: &sqlx::SqlitePool, focus_id: &str, rows: Vec<Row>) {
    seed_with_tier(pool, focus_id, "local_only", rows).await
}

async fn seed_with_tier(pool: &sqlx::SqlitePool, focus_id: &str, tier: &str, rows: Vec<Row>) {
    sqlx::query(
        "INSERT INTO focuses
            (focus_id, display_name, description, version, max_routing_tier,
             output_type, suggest_in_focuses, multi_source_validation,
             generic_title_template, status, created_at, updated_at)
         VALUES (?, 'Block Guard Focus', '', '1.0', ?, 'general', '[]', 0,
                 'Hidden', 'shipped', datetime('now'), datetime('now'))",
    )
    .bind(focus_id)
    .bind(tier)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO focus_launch_questions
           (focus_id, question_id, slot, kind, sort_order, asked, default_option, prompt)
         VALUES (?, 'kind', 'document_type', 'choice', 0, 0, 'essay_section', 'Which kind?')",
    )
    .bind(focus_id)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO focus_launch_options
           (focus_id, question_id, option_id, sort_order, label, hint)
         VALUES (?, 'kind', 'essay_section', 0, 'Essay', '')",
    )
    .bind(focus_id)
    .execute(pool)
    .await
    .unwrap();
    for (seq, r) in rows.iter().enumerate() {
        let comp = format!("c-{focus_id}-{}", r.id);
        sqlx::query(
            "INSERT INTO focus_block_compositions
                (id, focus_id, action_id, block_stable_id, sequence_index,
                 guard_var, guard_op, guard_clauses, created_at, updated_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, datetime('now'), datetime('now'))",
        )
        .bind(&comp)
        .bind(focus_id)
        .bind(r.id)
        .bind(r.block)
        .bind(seq as i64)
        .bind(r.guard.map(|g| g.0))
        .bind(r.guard.map(|g| g.1))
        .bind(r.clauses.as_ref().map(Value::to_string))
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO focus_block_customization
                (id, composition_id, persona_id, customization, created_at, updated_at)
             VALUES (?, ?, NULL, ?, datetime('now'), datetime('now'))",
        )
        .bind(format!("d-{comp}"))
        .bind(&comp)
        .bind(r.custom.to_string())
        .execute(pool)
        .await
        .unwrap();
    }
}

fn draft(brief_var: &str, out: &str) -> Value {
    json!({
        "common": {
            "display_name": "Drafting",
            "output_var": out,
            "need": { "task_type": "creative_writing", "difficulty": "low", "output_size": "short" },
            "declared_inputs": [brief_var],
            "range": {
                "quality_floor": { "min": "basic", "max": "standard" },
                "highest_tier": "local_only",
                "destinations": ["local"]
            },
            "class_preference": null
        },
        "spec": {
            "document_type": "essay_section",
            "brief_var": brief_var,
            "sender_var": null,
            "context_var": null
        }
    })
}

fn plain(out: &str, template: &str) -> Value {
    json!({"routing_tier": 1, "task_type": "quick_response", "output_var": out,
           "prompt_template": template})
}

fn check_row(sources: Value, run: Option<Value>) -> Value {
    let mut dc = json!({
        "document_type": "essay_section", "emit": "verdict", "attempt": 0, "sources": sources
    });
    if let Some(r) = run {
        dc["run"] = r;
    }
    json!({"input_var": "draft_text", "output_var": "check_verdict", "criteria": [],
           "on_fail": "advisory", "draft_check": dc})
}

fn present(var: &str) -> Guard {
    Guard {
        var: var.to_owned(),
        condition: Condition::Present,
    }
}

async fn load(pool: &sqlx::SqlitePool, id: &str) -> Result<FocusDefinition, String> {
    match load_focus_definition_from_db(pool, id).await {
        Ok(Some(d)) => Ok(d),
        Ok(None) => Err("no focus".to_owned()),
        Err(LifecycleError::ValidationFailed(m)) => Err(m),
        Err(e) => Err(e.to_string()),
    }
}

fn step<'a>(d: &'a FocusDefinition, id: &str) -> &'a ActionDefinition {
    d.steps.iter().find(|s| s.action_id == id).expect(id)
}

/// A filler row after the draft so no row is last but a model step.
fn tail() -> Row {
    row("tail", None, plain("tail_out", "Done."))
}

#[tokio::test]
async fn a_draft_from_an_artifact_carries_its_own_present_clause() {
    with_root(async {
        let p = pool().await;
        seed(
            &p,
            "bg-a",
            vec![
                row("draft", Some("cb-12"), draft("content_brief", "draft_text")),
                tail(),
            ],
        )
        .await;
        let d = load(&p, "bg-a").await.unwrap();
        let s = step(&d, "draft");
        assert_eq!(s.guard, None);
        assert_eq!(s.and_guards, vec![present("content_brief")]);
        // The guard analysis sees it: a path without the brief skips the draft.
        let paths = composition_paths(&d.steps).unwrap();
        assert!(paths.len() >= 2, "{paths:?}");
    })
    .await;
}

#[tokio::test]
async fn a_draft_from_an_earlier_rows_output_gets_no_clause() {
    with_root(async {
        let p = pool().await;
        seed(
            &p,
            "bg-b",
            vec![
                row(
                    "brief",
                    None,
                    plain("brief_text", "Brief from {user_input}."),
                ),
                row("draft", Some("cb-12"), draft("brief_text", "draft_text")),
                tail(),
            ],
        )
        .await;
        let d = load(&p, "bg-b").await.unwrap();
        assert!(step(&d, "draft").and_guards.is_empty());
    })
    .await;
}

#[tokio::test]
async fn the_rows_clauses_follow_the_block_clause_and_a_restated_one_is_not_repeated() {
    with_root(async {
        let p = pool().await;
        let mut r1 = row("draft", Some("cb-12"), draft("content_brief", "draft_text"));
        r1.clauses = Some(json!([{"var": "handoff_pending", "op": "absent"}]));
        let mut r2 = row("draft2", Some("cb-12"), draft("content_brief", "draft_two"));
        r2.guard = Some(("content_brief", "present"));
        let mut r3 = row(
            "draft3",
            Some("cb-12"),
            draft("content_brief", "draft_three"),
        );
        r3.clauses = Some(json!([
            {"var": "handoff_pending", "op": "absent"},
            {"var": "content_brief", "op": "present"}
        ]));
        seed(&p, "bg-c", vec![r1, r2, r3, tail()]).await;
        let d = load(&p, "bg-c").await.unwrap();
        assert_eq!(
            step(&d, "draft").and_guards,
            vec![present("content_brief"), {
                Guard {
                    var: "handoff_pending".to_owned(),
                    condition: Condition::Absent,
                }
            }]
        );
        // Stated as the single guard: the block clause is not repeated.
        assert_eq!(step(&d, "draft2").guard, Some(present("content_brief")));
        assert!(step(&d, "draft2").and_guards.is_empty());
        // Stated in the list: kept once, in the row's place.
        assert_eq!(step(&d, "draft3").and_guards.len(), 2);
    })
    .await;
}

#[tokio::test]
async fn the_block_clause_counts_toward_the_clause_cap() {
    with_root(async {
        let p = pool().await;
        let mut r = row("draft", Some("cb-12"), draft("content_brief", "draft_text"));
        r.clauses = Some(json!([
            {"var": "handoff_pending", "op": "absent"},
            {"var": "handoff_received", "op": "absent"},
            {"var": "voice_guidance", "op": "absent"},
            {"var": "run_change", "op": "absent"}
        ]));
        seed(&p, "bg-d", vec![r, tail()]).await;
        let m = load(&p, "bg-d").await.unwrap_err();
        assert!(m.contains("required-input guard"), "{m}");
        assert!(m.contains("at most 4"), "{m}");
    })
    .await;
}

fn resolved(document_type: &str) -> Resolved {
    let t = registry().unwrap().document_type(document_type).unwrap();
    Resolved {
        resolution: LaunchResolution {
            values: BTreeMap::new(),
            mode: None,
            document_type: Some(document_type.to_owned()),
            settings: Some(t.settings.clone()),
        },
        mode_envelope: None,
    }
}

#[tokio::test]
async fn re_expansion_keeps_the_merged_clauses() {
    with_root(async {
        let p = pool().await;
        let mut r = row("draft", Some("cb-12"), draft("content_brief", "draft_text"));
        r.clauses = Some(json!([{"var": "handoff_pending", "op": "absent"}]));
        seed(&p, "bg-e", vec![r, tail()]).await;
        let mut d = load(&p, "bg-e").await.unwrap();
        let before = step(&d, "draft").and_guards.clone();
        reexpand(&mut d, &resolved("essay_section")).unwrap();
        assert_eq!(step(&d, "draft").and_guards, before);
        assert_eq!(before[0], present("content_brief"));
        // A loaded row that somehow lost the block clause is refused at re-expansion.
        let slot = d.steps.iter_mut().find(|s| s.action_id == "draft").unwrap();
        slot.and_guards.remove(0);
        let e = reexpand(&mut d, &resolved("essay_section")).unwrap_err();
        assert!(e.to_string().contains("required-input guard"), "{e}");
    })
    .await;
}

#[tokio::test]
async fn a_draft_check_row_declares_its_sources_and_gets_the_run_settings() {
    with_root(async {
        let p = pool().await;
        seed(
            &p,
            "bg-f",
            vec![
                row("draft", Some("cb-12"), draft("content_brief", "draft_text")),
                row(
                    "check",
                    Some("cb-06"),
                    check_row(json!([{"var": "content_brief"}]), None),
                ),
                tail(),
            ],
        )
        .await;
        let mut d = load(&p, "bg-f").await.unwrap();
        assert_eq!(step(&d, "check").declared_inputs, ["content_brief"]);
        assert!(crate::conductor::seeding::required_names(&d.steps).contains(&"content_brief"));
        assert!(
            step(&d, "check").block_customization.as_ref().unwrap()["draft_check"]
                .get("run")
                .is_none()
        );
        reexpand(&mut d, &resolved("essay_section")).unwrap();
        let run = &step(&d, "check").block_customization.as_ref().unwrap()["draft_check"]["run"];
        assert_eq!(run["document_type"], "essay_section");
        assert_eq!(run["latitude"], "strict");
        assert_eq!(run["pacing"], "unhurried");
    })
    .await;
}

#[tokio::test]
async fn a_draft_check_row_is_refused_for_an_authored_run_or_an_unknown_source() {
    with_root(async {
        let p = pool().await;
        seed(
            &p,
            "bg-g",
            vec![
                row("draft", Some("cb-12"), draft("content_brief", "draft_text")),
                row(
                    "check",
                    Some("cb-06"),
                    check_row(
                        json!([{"var": "content_brief"}]),
                        Some(json!({"document_type": "essay_section", "latitude": "free",
                                    "pacing": "brisk"})),
                    ),
                ),
                tail(),
            ],
        )
        .await;
        let m = load(&p, "bg-g").await.unwrap_err();
        assert!(m.contains("may not be authored"), "{m}");

        seed(
            &p,
            "bg-h",
            vec![
                row("draft", Some("cb-12"), draft("content_brief", "draft_text")),
                row(
                    "check",
                    Some("cb-06"),
                    check_row(json!([{"var": "nothing_makes_this"}]), None),
                ),
                tail(),
            ],
        )
        .await;
        let m = load(&p, "bg-h").await.unwrap_err();
        assert!(m.contains("nothing_makes_this"), "{m}");
    })
    .await;
}

#[test]
fn a_block_handler_is_given_only_its_declared_seeds() {
    let mut s = ActionDefinition {
        action_id: "check".to_owned(),
        display_name: "check".to_owned(),
        guide_id: "g".to_owned(),
        task_type: "general".to_owned(),
        routing_tier: ExternalAccess::LocalOnly,
        step_type: StepType::default(),
        output_var: None,
        prompt_template: String::new(),
        field_requirements: HashMap::new(),
        options_override: HashMap::new(),
        external_access_override: None,
        requires_user_handoff: false,
        revisitable: false,
        block_stable_id: Some("cb-06".to_owned()),
        block_customization: None,
        schedule_trigger: None,
        guard: None,
        and_guards: Vec::new(),
        declared_inputs: vec!["content_brief".to_owned()],
        store_op: None,
        requirement: None,
    };
    let seeds: HashMap<String, String> = [
        ("content_brief", "the brief"),
        ("voice_guidance", "not declared"),
        ("case_context", "not declared either"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v.to_owned()))
    .collect();
    let got = declared_seeds(&s, &seeds);
    assert_eq!(got.len(), 1);
    assert_eq!(got["content_brief"], "the brief");
    s.declared_inputs.clear();
    assert!(declared_seeds(&s, &seeds).is_empty());
}

/// The strict "regenerate once" shape (decisions.id=942 point 3): a second draft row and
/// a second check (attempt 1, which never regenerates) run only when the first check's
/// verdict is `regenerate`. The composition loads, both branches are paths, and the
/// second draft is on no path twice.
#[tokio::test]
async fn the_regenerate_row_runs_only_on_regenerate_and_at_most_once() {
    with_root(async {
        let p = pool().await;
        let mut again = row(
            "draft_again",
            Some("cb-12"),
            draft("content_brief", "draft_text"),
        );
        again.guard = Some(("check_verdict", "eq"));
        let mut second = row("check_again", Some("cb-06"), {
            let mut c = check_row(json!([{"var": "content_brief"}]), None);
            c["draft_check"]["attempt"] = json!(1);
            c["output_var"] = json!("check_verdict_again");
            c
        });
        second.guard = Some(("check_verdict", "eq"));
        // A check row is a pure block: it emits no clause of its own, so the composition
        // guards it on the brief like the draft it follows (unit C does the same).
        second.clauses = Some(json!([{"var": "content_brief", "op": "present"}]));
        let mut fallback = row("apply", None, plain("final_text", "Tidy {draft_text}."));
        fallback.guard = Some(("check_verdict", "present"));
        seed(
            &p,
            "bg-r",
            vec![
                row("draft", Some("cb-12"), draft("content_brief", "draft_text")),
                {
                    let mut c = row(
                        "check",
                        Some("cb-06"),
                        check_row(json!([{"var": "content_brief"}]), None),
                    );
                    c.guard = Some(("content_brief", "present"));
                    c
                },
                again,
                second,
                fallback,
                tail(),
            ],
        )
        .await;
        // The single guard's value column: both eq rows compare with "regenerate".
        sqlx::query(
            "UPDATE focus_block_compositions SET guard_value = 'regenerate'
             WHERE focus_id = 'bg-r' AND guard_op = 'eq'",
        )
        .execute(&p)
        .await
        .unwrap();
        let d = load(&p, "bg-r").await.unwrap();
        let paths = composition_paths(&d.steps).unwrap();
        // Row indexes: 0 draft, 1 check, 2 draft_again, 3 check_again, 4 apply.
        let idx = |id: &str| d.steps.iter().position(|s| s.action_id == id).unwrap();
        let (again, apply) = (idx("draft_again"), idx("apply"));
        assert!(paths.iter().any(|x| x.rows.contains(&again)));
        assert!(paths
            .iter()
            .any(|x| x.rows.contains(&apply) && !x.rows.contains(&again)));
        for x in &paths {
            assert!(x.rows.iter().filter(|&&r| r == again).count() <= 1);
            // The second check runs exactly when the second draft does.
            assert_eq!(
                x.rows.contains(&again),
                x.rows.contains(&idx("check_again"))
            );
        }
    })
    .await;
}

/// items.id=903 unit B: the chat's document privacy flag tightens the run's ceiling
/// (medical, financial, legal: local_only) and never loosens it; a run with no chat
/// reads nothing; focus_settings is unchanged. Read on the user's max_permitted after
/// the flag: the fixture Focus has no tier-manifest entry, so its own ceiling is
/// local_only whatever its row says (tier_manifest::apply_manifest).
#[tokio::test]
async fn the_document_flag_tightens_the_runs_effective_ceiling() {
    use crate::persistence::document_session_store::{self, DocumentPrivacyFlag};
    with_root(async {
        let p = pool().await;
        let key = "11".repeat(32);
        let user = uuid::Uuid::new_v4().to_string();
        crate::auth::user_store::create_user(
            &p,
            &user,
            "Doc User",
            "user",
            false,
            &[0u8; crate::auth::kdf::SALT_LEN],
            crate::auth::kdf::DEFAULT_ARGON2_MEMORY_KIB,
            crate::auth::kdf::DEFAULT_ARGON2_ITERATIONS,
            crate::auth::kdf::DEFAULT_ARGON2_PARALLELISM,
            &[0u8; 32],
        )
        .await
        .unwrap();
        let persona = uuid::Uuid::new_v4().to_string();
        crate::persistence::persona_store::create_persona(
            &p, &persona, "P", "personal", &user, &key, None,
        )
        .await
        .unwrap();
        seed_with_tier(&p, "bg-doc", "anonymous_required", vec![tail()]).await;
        crate::persistence::focus_settings_store::create_focus_settings(
            &p,
            &persona,
            "bg-doc",
            "bidirectional",
            "shared",
            2,
            ExternalAccess::AnonymousRequired,
            "open",
            None,
        )
        .await
        .unwrap();
        let run_in = |chat: Option<&str>| {
            let mut r: FocusRun = FocusRun::new(
                user.clone(),
                persona.clone(),
                "bg-doc".to_owned(),
                p.clone(),
                std::sync::Arc::new(crate::conductor::concurrency::ConductorScheduler::new()),
                String::new(),
                false,
                Some(key.clone()),
                None,
                false,
                std::collections::HashSet::new(),
                None,
            );
            r.context_key = chat.map(str::to_owned);
            r
        };
        let ceiling = |chat: Option<&'static str>| {
            let mut r = run_in(chat);
            async move {
                r.load().await.unwrap();
                r.resolve_tier_config().await.unwrap();
                (r._focus_max_permitted_tier, r.document_flag)
            }
        };
        assert_eq!(
            ceiling(Some("chat-d")).await,
            (ExternalAccess::AnonymousRequired, None)
        );
        let mut conn =
            crate::persistence::output_store::open_outputs_db_retrying(&user, &persona, &key)
                .await
                .unwrap();
        for (flag, want) in [
            (
                DocumentPrivacyFlag::General,
                ExternalAccess::AnonymousRequired,
            ),
            (
                DocumentPrivacyFlag::Personal,
                ExternalAccess::AnonymousRequired,
            ),
            (DocumentPrivacyFlag::Medical, ExternalAccess::LocalOnly),
            (DocumentPrivacyFlag::Financial, ExternalAccess::LocalOnly),
            (DocumentPrivacyFlag::Legal, ExternalAccess::LocalOnly),
        ] {
            document_session_store::set_privacy_flag(
                &mut conn,
                "chat-d",
                "bg-doc",
                "essay_section",
                flag,
            )
            .await
            .unwrap();
            assert_eq!(
                ceiling(Some("chat-d")).await,
                (want, Some(flag)),
                "{flag:?}"
            );
        }
        // items.id=903 unit D: a one-document raise of this document type lifts the
        // flagged document above the flag, and the Focus's own ceiling still clamps the
        // run (this fixture's manifest ceiling is local_only).
        document_session_store::set_raise(
            &mut conn,
            "chat-d",
            "bg-doc",
            "essay_section",
            ExternalAccess::AnonymousRequired,
            "audit-1",
        )
        .await
        .unwrap();
        let mut r = run_in(Some("chat-d"));
        r.load().await.unwrap();
        r.resolve_tier_config().await.unwrap();
        assert_eq!(
            r._focus_max_permitted_tier,
            ExternalAccess::AnonymousRequired
        );
        assert!(r._focus_external_access <= r.focus_def.as_ref().unwrap().max_routing_tier);
        // A raise for another document type is not this document's.
        let wrong = document_session_store::set_raise(
            &mut conn,
            "chat-d",
            "bg-doc",
            "rma_letter",
            ExternalAccess::AnonymousRequired,
            "audit-2",
        )
        .await;
        assert!(matches!(
            wrong,
            Err(document_session_store::DocumentSessionError::NoDocument)
        ));
        // Another chat and a run with no chat read no flag.
        assert_eq!(
            ceiling(Some("chat-e")).await.0,
            ExternalAccess::AnonymousRequired
        );
        assert_eq!(
            ceiling(None).await,
            (ExternalAccess::AnonymousRequired, None)
        );
        // focus_settings is unchanged.
        let s =
            crate::persistence::focus_settings_store::get_focus_settings(&p, &persona, "bg-doc")
                .await
                .unwrap()
                .unwrap();
        assert_eq!(s.max_permitted_tier, ExternalAccess::AnonymousRequired);
    })
    .await;
}
