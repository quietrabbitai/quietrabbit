// src-tauri/src/commands/step_review.rs
//
// items.id=884 (decisions.id=943 point 6, IPC Group 34): the two commands of the
// outbound review of an external step's prompt.
//
//   get_pending_step_review(persona_id, run_id)
//       The review a run is parked on, if any: its opaque id, the action, and the
//       flagged spans WITH their text when the process still holds it (state
//       "ready"), or no spans and state "needs_resume" when it does not (the app
//       restarted: a resume asks again). It exists so a front end that missed the
//       consent_request event (it mounted late, the run was started elsewhere)
//       can still show the review. READ ONLY.
//
//   list_pending_step_reviews(persona_id)
//       Every review of the persona whose run is still open and waiting for an
//       answer, oldest first, each as get_pending_step_review would give it. It is
//       what the host asks once on mount, so a review whose consent_request event
//       was missed (the app restarted, the host mounted late) is found without being
//       told which run to look at. READ ONLY.
//
//   decline_step_review(persona_id, run_id)
//       The user refused to send. The pending review becomes `refused` and the run
//       is cancelled, as for cancel_run (a use-local-instead path belongs to the
//       strategy chooser, items.id=900). Returns whether there was a pending
//       review to refuse. Without one nothing is cancelled.
//
// The answers themselves go through submit_element_consent_decision (unchanged)
// and resume_run. There is no command that supplies the text that is sent: the
// server applies the recorded answers to the text it rendered (conductor::
// step_review), and nothing here reads or returns the prompt.
//
// The same shape as the consent, device and hand-off commands: writer_gate::command
// first (a key rotation waits for it), the user and key from the unlocked session
// (no user_id argument to spoof), the Persona id a canonical UUID that belongs to
// that user, and the work in a *_for_session body with no Tauri state.
//
// ERROR CODES (ERROR_CODES, each with a placeholder key step_review.error.<code> in
// the frontend's en.json, a test requires it): persona_not_found, invalid_run_id,
// step_review_unavailable. The last is any store failure and carries no detail.
// (The list command takes no run id, so it answers only the first and the last.)

use serde::Deserialize;
use specta::Type;
use tauri::State;

use crate::auth::registry::KeyRegistry;
use crate::commands::persona::session_user_and_key;
use crate::conductor::step_review::{self, PendingStepReview};
use crate::persistence::{output_store, persona_store};

/// Every error string this module returns.
pub const ERROR_CODES: [&str; 3] = [
    "persona_not_found",
    "invalid_run_id",
    "step_review_unavailable",
];

#[derive(Debug, Clone, Deserialize, Type)]
pub struct StepReviewRunRequest {
    pub persona_id: String,
    pub run_id: String,
}

fn unavailable<E: std::fmt::Display>(what: &str, e: E) -> String {
    // The error is a store or validation message, never a stored value.
    log::warn!("step_review command: {what}: {e}");
    "step_review_unavailable".to_owned()
}

fn canonical_uuid(s: &str) -> bool {
    uuid::Uuid::parse_str(s)
        .map(|u| u.hyphenated().to_string() == s)
        .unwrap_or(false)
}

/// The Persona id must be a canonical UUID AND belong to the signed-in user. The
/// two failures answer alike. Private on purpose, like the other commands' copies,
/// so items.id=638 can lift it into one shared check.
async fn require_persona(
    pool: &sqlx::SqlitePool,
    user_id: &str,
    persona_id: &str,
) -> Result<(), String> {
    if !canonical_uuid(persona_id) {
        return Err("persona_not_found".to_owned());
    }
    match persona_store::is_user_in_persona(pool, user_id, persona_id).await {
        Ok(true) => Ok(()),
        Ok(false) => Err("persona_not_found".to_owned()),
        Err(e) => Err(unavailable("persona lookup", e)),
    }
}

/// A run id is a canonical UUID (every run, API Chat's included, mints one).
fn require_run_id(run_id: &str) -> Result<(), String> {
    if canonical_uuid(run_id) {
        Ok(())
    } else {
        Err("invalid_run_id".to_owned())
    }
}

#[tauri::command]
#[specta::specta]
pub async fn get_pending_step_review(
    request: StepReviewRunRequest,
    key_registry: State<'_, KeyRegistry>,
    pool: State<'_, sqlx::SqlitePool>,
) -> Result<Option<PendingStepReview>, String> {
    let _lease = crate::auth::writer_gate::command(&key_registry).await?;
    let (user_id, key_hex) = session_user_and_key(&key_registry).await?;
    get_pending_step_review_for_session(
        &pool,
        &user_id,
        &key_hex,
        &request.persona_id,
        &request.run_id,
    )
    .await
}

pub(crate) async fn get_pending_step_review_for_session(
    pool: &sqlx::SqlitePool,
    user_id: &str,
    key_hex: &str,
    persona_id: &str,
    run_id: &str,
) -> Result<Option<PendingStepReview>, String> {
    require_persona(pool, user_id, persona_id).await?;
    require_run_id(run_id)?;
    let mut conn = output_store::open_outputs_db_retrying(user_id, persona_id, key_hex)
        .await
        .map_err(|e| unavailable("open outputs.db", e))?;
    step_review::pending_view(&mut conn, run_id)
        .await
        .map_err(|e| unavailable("read pending review", e))
}

#[tauri::command]
#[specta::specta]
pub async fn list_pending_step_reviews(
    persona_id: String,
    key_registry: State<'_, KeyRegistry>,
    pool: State<'_, sqlx::SqlitePool>,
) -> Result<Vec<PendingStepReview>, String> {
    let _lease = crate::auth::writer_gate::command(&key_registry).await?;
    let (user_id, key_hex) = session_user_and_key(&key_registry).await?;
    list_pending_step_reviews_for_session(&pool, &user_id, &key_hex, &persona_id).await
}

pub(crate) async fn list_pending_step_reviews_for_session(
    pool: &sqlx::SqlitePool,
    user_id: &str,
    key_hex: &str,
    persona_id: &str,
) -> Result<Vec<PendingStepReview>, String> {
    require_persona(pool, user_id, persona_id).await?;
    let mut conn = output_store::open_outputs_db_retrying(user_id, persona_id, key_hex)
        .await
        .map_err(|e| unavailable("open outputs.db", e))?;
    step_review::pending_views(&mut conn)
        .await
        .map_err(|e| unavailable("list pending reviews", e))
}

#[tauri::command]
#[specta::specta]
pub async fn decline_step_review(
    request: StepReviewRunRequest,
    key_registry: State<'_, KeyRegistry>,
    pool: State<'_, sqlx::SqlitePool>,
) -> Result<bool, String> {
    let _lease = crate::auth::writer_gate::command(&key_registry).await?;
    let (user_id, key_hex) = session_user_and_key(&key_registry).await?;
    decline_step_review_for_session(
        &pool,
        &user_id,
        &key_hex,
        &request.persona_id,
        &request.run_id,
    )
    .await
}

pub(crate) async fn decline_step_review_for_session(
    pool: &sqlx::SqlitePool,
    user_id: &str,
    key_hex: &str,
    persona_id: &str,
    run_id: &str,
) -> Result<bool, String> {
    require_persona(pool, user_id, persona_id).await?;
    require_run_id(run_id)?;
    let refused = {
        let mut conn = output_store::open_outputs_db_retrying(user_id, persona_id, key_hex)
            .await
            .map_err(|e| unavailable("open outputs.db", e))?;
        step_review::decline_pending(&mut conn, run_id)
            .await
            .map_err(|e| unavailable("refuse pending review", e))?
    };
    if !refused {
        return Ok(false);
    }
    // The run ends cancelled (decisions.id=943 ruling Q4). A failure here leaves
    // the review refused and the run parked: the user can cancel it again.
    crate::commands::execution::cancel_run_for_session(
        user_id, user_id, persona_id, key_hex, run_id,
    )
    .await
    .map_err(|e| unavailable("cancel run", e))?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::registry::key_hex;
    use crate::conductor::step_review::{
        fake::{asked_span, FakeScanner},
        review_prompt, Park, ReviewDecision, ReviewInput, STATE_NEEDS_RESUME, STATE_READY,
    };
    use crate::persistence::step_review_store::{self as store, ReviewOutcome};
    use crate::test_support::ENV_MUTEX;

    const MASTER_KEY: [u8; crate::auth::kdf::MASTER_KEY_LEN] =
        [0xABu8; crate::auth::kdf::MASTER_KEY_LEN];
    const PROMPT: &str = "Write to Ann Smith about the invoice";

    struct Env {
        _tempdir: tempfile::TempDir,
        _lock: tokio::sync::MutexGuard<'static, ()>,
        saved_root: Option<String>,
        pool: sqlx::SqlitePool,
        user: String,
        persona: String,
        key: String,
    }

    impl Drop for Env {
        fn drop(&mut self) {
            match &self.saved_root {
                Some(v) => std::env::set_var("QR_DATA_ROOT", v),
                None => std::env::remove_var("QR_DATA_ROOT"),
            }
        }
    }

    async fn env() -> Env {
        let lock = ENV_MUTEX.lock().await;
        let saved_root = std::env::var("QR_DATA_ROOT").ok();
        let tempdir = tempfile::tempdir().expect("tempdir");
        std::env::set_var("QR_DATA_ROOT", tempdir.path());
        crate::persistence::migrations::migrate_shared_db()
            .await
            .expect("shared.db");
        let pool =
            sqlx::SqlitePool::connect_with(crate::providers::utils::connect_options_unencrypted(
                &crate::providers::utils::db_path_shared(),
            ))
            .await
            .expect("pool");
        let key = key_hex(&MASTER_KEY);
        let user = uuid::Uuid::new_v4().to_string();
        crate::auth::user_store::create_user(
            &pool,
            &user,
            "Review User",
            "user",
            false,
            &[0u8; crate::auth::kdf::SALT_LEN],
            crate::auth::kdf::DEFAULT_ARGON2_MEMORY_KIB,
            crate::auth::kdf::DEFAULT_ARGON2_ITERATIONS,
            crate::auth::kdf::DEFAULT_ARGON2_PARALLELISM,
            &[0u8; 32],
        )
        .await
        .expect("create_user");
        let persona = uuid::Uuid::new_v4().to_string();
        persona_store::create_persona(&pool, &persona, "P", "personal", &user, &key, None)
            .await
            .expect("create_persona");
        Env {
            _tempdir: tempdir,
            _lock: lock,
            saved_root,
            pool,
            user,
            persona,
            key,
        }
    }

    async fn conn(e: &Env) -> sqlx::SqliteConnection {
        output_store::open_outputs_db(&e.user, &e.persona, &e.key)
            .await
            .expect("outputs.db")
    }

    async fn run_row(e: &Env, run_id: &str) {
        let mut c = conn(e).await;
        sqlx::query(
            "INSERT INTO focus_runs (id, focus_id, status, started_at)
             VALUES (?, 'writing-assistant', 'awaiting_user', 't')",
        )
        .bind(run_id)
        .execute(&mut c)
        .await
        .unwrap();
    }

    /// Park one review on `run_id` with one flagged span ("Ann Smith").
    async fn park(e: &Env, run_id: &str) -> String {
        let mut c = conn(e).await;
        let scanner = FakeScanner::pending(vec![asked_span("s1", PROMPT, "Ann Smith")]);
        let input = ReviewInput {
            run_id,
            action_id: "draft",
            prompt: PROMPT,
            provider_id: Some("prov"),
        };
        match review_prompt(
            &mut c,
            &scanner,
            &crate::conductor::step_review::ReviewReuse::default(),
            &input,
        )
        .await
        {
            ReviewDecision::Park(Park::Pending { review_id }) => review_id,
            other => panic!("expected a pending review, got {other:?}"),
        }
    }

    async fn get(e: &Env, persona: &str, run: &str) -> Result<Option<PendingStepReview>, String> {
        get_pending_step_review_for_session(&e.pool, &e.user, &e.key, persona, run).await
    }

    #[tokio::test]
    async fn nothing_pending_answers_none_and_declining_cancels_nothing() {
        let e = env().await;
        let run = uuid::Uuid::new_v4().to_string();
        run_row(&e, &run).await;
        assert!(get(&e, &e.persona, &run).await.unwrap().is_none());
        assert!(
            !decline_step_review_for_session(&e.pool, &e.user, &e.key, &e.persona, &run)
                .await
                .unwrap()
        );
        let status = output_store::get_focus_run_status(&e.user, &e.persona, &e.key, &run)
            .await
            .unwrap();
        assert_eq!(status.as_deref(), Some("awaiting_user"), "not cancelled");
    }

    #[tokio::test]
    async fn a_parked_review_is_shown_with_its_spans_while_the_text_is_held() {
        let e = env().await;
        let run = uuid::Uuid::new_v4().to_string();
        run_row(&e, &run).await;
        let id = park(&e, &run).await;

        let view = get(&e, &e.persona, &run).await.unwrap().unwrap();
        assert_eq!(view.review_id, id);
        assert_eq!(view.run_id, run);
        assert_eq!(view.action_id, "draft");
        assert_eq!(view.state, STATE_READY);
        assert_eq!(view.spans.len(), 1);
        assert_eq!(view.spans[0].original_text, "Ann Smith");
        assert_eq!(view.spans[0].span_id, "s1");
    }

    #[tokio::test]
    async fn after_a_restart_the_review_is_reported_as_needing_a_resume() {
        let e = env().await;
        let run = uuid::Uuid::new_v4().to_string();
        run_row(&e, &run).await;
        // A pending row whose text is not held: the shape a restart leaves.
        {
            let mut c = conn(&e).await;
            let spans = store::StoredSpans {
                asked: vec![store::AskedSpan {
                    span_id: "gone".into(),
                    start_byte: 9,
                    end_byte: 18,
                    category: "private_person".into(),
                    review_tier: "high".into(),
                }],
                settled: vec![],
            };
            store::insert_review(
                &mut c,
                &store::NewReview {
                    focus_run_id: &run,
                    action_id: "draft",
                    prompt_sha: &step_review::sha256_hex(PROMPT),
                    prompt_len: PROMPT.len(),
                    spans: &spans,
                    counts: store::Counts::default(),
                    outcome: ReviewOutcome::Pending,
                    reason: None,
                    outbound_sha: None,
                    outbound_len: None,
                    provider_id: None,
                },
            )
            .await
            .unwrap();
        }
        let view = get(&e, &e.persona, &run).await.unwrap().unwrap();
        assert_eq!(view.state, STATE_NEEDS_RESUME);
        assert!(view.spans.is_empty(), "no text without the held spans");
    }

    async fn list(e: &Env, persona: &str) -> Result<Vec<PendingStepReview>, String> {
        list_pending_step_reviews_for_session(&e.pool, &e.user, &e.key, persona).await
    }

    #[tokio::test]
    async fn the_list_is_empty_with_nothing_pending() {
        let e = env().await;
        assert!(list(&e, &e.persona).await.unwrap().is_empty());
        let run = uuid::Uuid::new_v4().to_string();
        run_row(&e, &run).await;
        assert!(list(&e, &e.persona).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn the_list_holds_every_open_pending_review_oldest_first() {
        let e = env().await;
        let first = uuid::Uuid::new_v4().to_string();
        let second = uuid::Uuid::new_v4().to_string();
        run_row(&e, &first).await;
        run_row(&e, &second).await;
        let id1 = park(&e, &first).await;
        let id2 = park(&e, &second).await;

        let listed = list(&e, &e.persona).await.unwrap();
        let ids: Vec<&str> = listed.iter().map(|r| r.review_id.as_str()).collect();
        assert_eq!(ids, [id1.as_str(), id2.as_str()]);
        assert_eq!(listed[0].run_id, first);
        assert_eq!(listed[1].run_id, second);
        // The text is held, so each is ready with its spans, as the per-run read says.
        for r in &listed {
            assert_eq!(r.state, STATE_READY);
            assert_eq!(r.spans[0].original_text, "Ann Smith");
        }
        let single = get(&e, &e.persona, &first).await.unwrap().unwrap();
        assert_eq!(single.review_id, listed[0].review_id);
        assert_eq!(single.state, listed[0].state);
    }

    #[tokio::test]
    async fn the_list_omits_ended_runs_and_answered_reviews() {
        let e = env().await;
        let open = uuid::Uuid::new_v4().to_string();
        let ended = uuid::Uuid::new_v4().to_string();
        let declined = uuid::Uuid::new_v4().to_string();
        for r in [&open, &ended, &declined] {
            run_row(&e, r).await;
        }
        let keep = park(&e, &open).await;
        park(&e, &ended).await;
        park(&e, &declined).await;
        // One run failed without cancelling (its row is still pending), one review
        // was refused.
        let mut c = conn(&e).await;
        sqlx::query("UPDATE focus_runs SET status = 'failed' WHERE id = ?")
            .bind(&ended)
            .execute(&mut c)
            .await
            .unwrap();
        drop(c);
        assert!(
            decline_step_review_for_session(&e.pool, &e.user, &e.key, &e.persona, &declined)
                .await
                .unwrap()
        );
        let listed = list(&e, &e.persona).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].review_id, keep);
    }

    #[tokio::test]
    async fn the_list_reports_needs_resume_when_the_text_is_not_held() {
        let e = env().await;
        let run = uuid::Uuid::new_v4().to_string();
        run_row(&e, &run).await;
        let mut c = conn(&e).await;
        let spans = store::StoredSpans {
            asked: vec![store::AskedSpan {
                span_id: "gone".into(),
                start_byte: 9,
                end_byte: 18,
                category: "private_person".into(),
                review_tier: "high".into(),
            }],
            settled: vec![],
        };
        store::insert_review(
            &mut c,
            &store::NewReview {
                focus_run_id: &run,
                action_id: "draft",
                prompt_sha: &step_review::sha256_hex(PROMPT),
                prompt_len: PROMPT.len(),
                spans: &spans,
                counts: store::Counts::default(),
                outcome: ReviewOutcome::Pending,
                reason: None,
                outbound_sha: None,
                outbound_len: None,
                provider_id: None,
            },
        )
        .await
        .unwrap();
        drop(c);
        let listed = list(&e, &e.persona).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].state, STATE_NEEDS_RESUME);
        assert!(listed[0].spans.is_empty());
    }

    #[tokio::test]
    async fn the_list_checks_the_persona_and_never_reads_another_users() {
        let e = env().await;
        let run = uuid::Uuid::new_v4().to_string();
        run_row(&e, &run).await;
        park(&e, &run).await;
        let other_user = uuid::Uuid::new_v4().to_string();
        assert_eq!(
            list(&e, "persona-1").await.unwrap_err(),
            "persona_not_found"
        );
        assert_eq!(
            list(&e, &e.persona.to_uppercase()).await.unwrap_err(),
            "persona_not_found"
        );
        assert_eq!(
            list_pending_step_reviews_for_session(&e.pool, &other_user, &e.key, &e.persona)
                .await
                .unwrap_err(),
            "persona_not_found"
        );
        // Another persona of the same user has its own file and sees none of it.
        let second = uuid::Uuid::new_v4().to_string();
        persona_store::create_persona(&e.pool, &second, "Q", "personal", &e.user, &e.key, None)
            .await
            .unwrap();
        assert!(list(&e, &second).await.unwrap().is_empty());
        assert_eq!(list(&e, &e.persona).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn declining_refuses_the_review_and_cancels_the_run() {
        let e = env().await;
        let run = uuid::Uuid::new_v4().to_string();
        run_row(&e, &run).await;
        let id = park(&e, &run).await;

        assert!(
            decline_step_review_for_session(&e.pool, &e.user, &e.key, &e.persona, &run)
                .await
                .unwrap()
        );
        let mut c = conn(&e).await;
        let rows = store::list_reviews(&mut c, &run, "draft").await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].review_id, id);
        assert_eq!(rows[0].outcome, ReviewOutcome::Refused);
        let status = output_store::get_focus_run_status(&e.user, &e.persona, &e.key, &run)
            .await
            .unwrap();
        assert_eq!(status.as_deref(), Some("cancelled"));
        // Nothing is pending any more, and a second decline does nothing.
        assert!(get(&e, &e.persona, &run).await.unwrap().is_none());
        assert!(
            !decline_step_review_for_session(&e.pool, &e.user, &e.key, &e.persona, &run)
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn the_persona_and_run_ids_are_checked_and_answer_with_a_code() {
        let e = env().await;
        let run = uuid::Uuid::new_v4().to_string();
        run_row(&e, &run).await;
        let other_user = uuid::Uuid::new_v4().to_string();
        let results = [
            get(&e, "persona-1", &run).await,
            get(&e, &e.persona.to_uppercase(), &run).await,
            get_pending_step_review_for_session(&e.pool, &other_user, &e.key, &e.persona, &run)
                .await,
            get(&e, &e.persona, "not-a-uuid").await,
            get(&e, &e.persona, &run.to_uppercase()).await,
            get(&e, &e.persona, "").await,
        ];
        let expect = [
            "persona_not_found",
            "persona_not_found",
            "persona_not_found",
            "invalid_run_id",
            "invalid_run_id",
            "invalid_run_id",
        ];
        for (r, want) in results.iter().zip(expect) {
            let err = r.as_ref().unwrap_err();
            assert_eq!(err, want);
            assert!(ERROR_CODES.contains(&err.as_str()));
        }
        let declined =
            decline_step_review_for_session(&e.pool, &e.user, &e.key, "nope", &run).await;
        assert_eq!(declined.unwrap_err(), "persona_not_found");
    }

    /// Every error code has a placeholder key in the frontend's en.json (Chat-BRAND
    /// owns the wording), and no key lacks a code.
    #[test]
    fn every_error_code_has_a_placeholder_key_in_en_json() {
        let json: serde_json::Value =
            serde_json::from_str(include_str!("../../../frontend/src/i18n/en.json")).unwrap();
        for code in ERROR_CODES {
            let text = json["step_review"]["error"][code]
                .as_str()
                .unwrap_or_else(|| panic!("en.json has no step_review.error.{code}"));
            assert!(!text.trim().is_empty(), "{code}");
        }
        let keys: Vec<&String> = json["step_review"]["error"]
            .as_object()
            .unwrap()
            .keys()
            .collect();
        assert_eq!(keys.len(), ERROR_CODES.len(), "{keys:?}");
    }

    /// The list is read only: its body names no writer.
    #[test]
    fn the_list_command_is_read_only() {
        let src = include_str!("step_review.rs");
        let body = src.split("#[cfg(test)]").next().unwrap();
        let start = body
            .find("pub(crate) async fn list_pending_step_reviews_for_session")
            .unwrap();
        let end = body
            .find("#[tauri::command]\n#[specta::specta]\npub async fn decline_step_review")
            .unwrap();
        let list = &body[start..end];
        for banned in [
            "decline_pending",
            "cancel_run",
            "INSERT",
            "UPDATE",
            "DELETE",
            "resume",
        ] {
            assert!(!list.contains(banned), "the list command names {banned}");
        }
        // The command itself takes the writer gate first, like its siblings.
        let cmd_start = body
            .find("pub async fn list_pending_step_reviews(")
            .unwrap();
        let cmd = &body[cmd_start..start];
        assert!(cmd.contains("crate::auth::writer_gate::command(&key_registry).await?"));
        assert!(cmd.contains("session_user_and_key(&key_registry)"));
    }

    /// The commands never read or return a prompt: production code names no
    /// renderer and returns nothing but the review's own view.
    #[test]
    fn this_module_never_touches_prompt_text() {
        let src = include_str!("step_review.rs");
        let body = src.split("#[cfg(test)]").next().unwrap();
        let code: String = body
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        for banned in [
            "render_template",
            "render_prompt",
            "hosted_step_call",
            "ReviewedPrompt",
            "outbound_for",
            "review_prompt",
            "write_element_consent_decisions",
        ] {
            assert!(
                !code.contains(banned),
                "commands::step_review names {banned}"
            );
        }
    }
}
