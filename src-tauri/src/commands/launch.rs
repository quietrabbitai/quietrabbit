// src-tauri/src/commands/launch.rs
//
// items.id=897 stage 4 (decisions.id=942 point 8, decisions.id=948 point 4): the
// LAUNCH COMMANDS, for the generic Focus screen (items.id=902). Three commands, no
// screen:
//
//   get_focus_launch_config(focus_id)
//       SHIPPED DATA ONLY, read only: the Focus's display name, description and
//       one-line description (conductor::focus_display, the one reader), and its
//       launch questions with their options and defaults. The mode list is the
//       options of the question of kind `mode`. A mode's PROFILE (what it narrows)
//       is never returned: the screen shows labels, and the plan screen
//       (items.id=901) is where resources are shown. A Focus with no question
//       returns an empty list.
//   get_launch_state(persona_id, context_key, focus_id)
//       what this chat has answered and what applies now: per question the stored
//       answer, the effective value (the answer, or the default) and whether it is
//       the default, plus the per-run change and whether the chat is one-time. READ
//       ONLY, so it works in a latched chat too (the latch refuses writes only).
//   submit_launch_answers(persona_id, context_key, focus_id, answers, run_change)
//       validates every answer against the shipped data (ids only, no free text, no
//       unasked question, run_change parsed as a settings patch whose steps exist)
//       and writes them in one transaction through the typed launch-answer store.
//       Returns the new state.
//
// WHAT THIS NEVER DOES: approve a hosted or metered step (decisions.id=942 Q3: that
// always takes one explicit plan approval), widen a ceiling, or switch a gate off.
// An answer only chooses among options the Focus shipped, and a mode only narrows
// (conductor::run_settings). A source scan pins that this module never names the
// spend gate, a provider or a plan.
//
// THE ONE-TIME LATCH (decisions.id=939 section 4, decisions.id=948 point 3): obeyed
// with no exemption. A submit in a latched chat is refused WHOLE with its own code,
// `one_time_chat`, and the run uses the Focus defaults (answers stored before the
// latch still apply). The state carries `one_time_chat` and `defaults_apply` so the
// screen can say so; the wording is Chat-BRAND's (items.id=506), placeholder keys
// only. A latch that cannot be read fails the submit closed (`launch_unavailable`).
//
// The same shape as the consent, device and hand-off commands: writer_gate::command
// first (a key rotation waits for it), the user and key from the unlocked session
// (session_user_and_key: no user_id argument to spoof), the Persona id must be a
// canonical UUID that belongs to that user, and the work is in a *_for_session body
// with no Tauri state, tested directly. The context key is the chat's, validated; the
// server uses it only for this chat's rows.
//
// ERROR CODES (ERROR_CODES, each with a placeholder key launch.error.<code> in the
// frontend's en.json, a test requires it): persona_not_found, invalid_focus_id,
// invalid_context_key, no_launch_config, unknown_question, question_not_asked,
// invalid_answer, invalid_run_change, one_time_chat, launch_unavailable. The last is
// any store failure or a stored value that fails its validator, and carries no
// detail. No code or log line holds an answer.

use serde::{Deserialize, Serialize};
use specta::Type;
use tauri::State;

use crate::auth::registry::KeyRegistry;
use crate::commands::persona::session_user_and_key;
use crate::conductor::bound_spec;
use crate::conductor::focus_display::{self, FocusDisplayError};
use crate::conductor::launch_config::{self, LaunchConfig, QuestionKind};
use crate::conductor::lifecycle::LifecycleError;
use crate::conductor::model_block::document_type::registry;
use crate::persistence::launch_answer_store::{self, PutError};
use crate::persistence::{output_store, persona_store, session_store};

/// Every error string this module returns.
pub const ERROR_CODES: [&str; 10] = [
    "persona_not_found",
    "invalid_focus_id",
    "invalid_context_key",
    "no_launch_config",
    "unknown_question",
    "question_not_asked",
    "invalid_answer",
    "invalid_run_change",
    "one_time_chat",
    "launch_unavailable",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum LaunchQuestionKindView {
    /// Its options are the Focus's modes.
    Mode,
    Choice,
}

/// One option of a question. Placeholder wording, Chat-BRAND's.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Type)]
pub struct LaunchOptionView {
    pub id: String,
    pub label: String,
    pub hint: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Type)]
pub struct LaunchQuestionView {
    pub id: String,
    pub kind: LaunchQuestionKindView,
    /// false: a preset that fixes the value. Show nothing to answer.
    pub asked: bool,
    pub prompt: String,
    pub default_option: String,
    pub options: Vec<LaunchOptionView>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Type)]
pub struct FocusLaunchConfig {
    pub focus_id: String,
    pub display_name: String,
    pub description: String,
    /// The optional short line for a one-line row.
    pub ui_one_liner: Option<String>,
    pub questions: Vec<LaunchQuestionView>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Type)]
pub struct LaunchQuestionState {
    pub question_id: String,
    /// The stored answer, if one is stored and the question is asked.
    pub answer: Option<String>,
    /// What applies now: the answer, or the question's default.
    pub effective: String,
    pub from_default: bool,
    /// An answer is stored but is no longer one of the question's options. A run
    /// would stop on it: ask the question again.
    pub stale: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Type)]
pub struct LaunchState {
    pub focus_id: String,
    pub questions: Vec<LaunchQuestionState>,
    /// The stored per-run change (a small JSON object of the visible settings).
    pub run_change: Option<String>,
    /// The chat is a one-time session: nothing is saved in it, so a submit is
    /// refused with `one_time_chat`.
    pub one_time_chat: bool,
    /// One-time chat and no answer was stored before it began: the Focus defaults
    /// apply. (Answers stored before the latch still apply.)
    pub defaults_apply: bool,
}

/// One answer in a submit: ids only, no free text.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Type)]
pub struct LaunchAnswerInput {
    pub question_id: String,
    pub value: String,
}

fn unavailable<E: std::fmt::Display>(what: &str, e: E) -> String {
    // The error is a store or validation message, never an answer.
    log::warn!("launch: {what}: {e}");
    "launch_unavailable".to_owned()
}

/// The Persona id must be a canonical UUID AND belong to the signed-in user. The
/// two failures answer alike. Private on purpose, like the other commands' copies,
/// so items.id=638 can lift it into one shared check.
async fn require_persona(
    pool: &sqlx::SqlitePool,
    user_id: &str,
    persona_id: &str,
) -> Result<(), String> {
    let canonical = uuid::Uuid::parse_str(persona_id)
        .map(|u| u.hyphenated().to_string() == persona_id)
        .unwrap_or(false);
    if !canonical {
        return Err("persona_not_found".to_owned());
    }
    match persona_store::is_user_in_persona(pool, user_id, persona_id).await {
        Ok(true) => Ok(()),
        Ok(false) => Err("persona_not_found".to_owned()),
        Err(e) => Err(unavailable("persona lookup", e)),
    }
}

/// A Focus slug: lowercase letters, digits, '-' and '_', 1 to 64 characters.
fn require_focus_id(focus_id: &str) -> Result<(), String> {
    let ok = !focus_id.is_empty()
        && focus_id.len() <= 64
        && focus_id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_');
    if ok {
        Ok(())
    } else {
        Err("invalid_focus_id".to_owned())
    }
}

fn require_context_key(context_key: &str) -> Result<(), String> {
    session_store::validate_context_key(context_key).map_err(|_| "invalid_context_key".to_owned())
}

/// The Focus's validated launch config, through the one validating reader.
async fn read_config(
    pool: &sqlx::SqlitePool,
    focus_id: &str,
) -> Result<Option<LaunchConfig>, String> {
    let reg = registry().map_err(|e| unavailable("document types", e))?;
    launch_config::read_validated(pool, focus_id, reg)
        .await
        .map_err(|e| unavailable("launch config", e))
}

// ---------------------------------------------------------------------------
// get_focus_launch_config
// ---------------------------------------------------------------------------

#[tauri::command]
#[specta::specta]
pub async fn get_focus_launch_config(
    focus_id: String,
    key_registry: State<'_, KeyRegistry>,
    pool: State<'_, sqlx::SqlitePool>,
) -> Result<FocusLaunchConfig, String> {
    let _lease = crate::auth::writer_gate::command(&key_registry).await?;
    session_user_and_key(&key_registry).await?;
    get_focus_launch_config_for_session(&pool, &focus_id).await
}

pub(crate) async fn get_focus_launch_config_for_session(
    pool: &sqlx::SqlitePool,
    focus_id: &str,
) -> Result<FocusLaunchConfig, String> {
    require_focus_id(focus_id)?;
    let display = focus_display::read(pool, focus_id)
        .await
        .map_err(|e| match e {
            FocusDisplayError::Load(LifecycleError::FocusNotFound(_)) => {
                "invalid_focus_id".to_owned()
            }
            other => unavailable("focus display", other),
        })?;
    let questions = match read_config(pool, focus_id).await? {
        None => Vec::new(),
        Some(c) => c
            .questions
            .iter()
            .map(|q| LaunchQuestionView {
                id: q.id.clone(),
                kind: match q.kind {
                    QuestionKind::Mode => LaunchQuestionKindView::Mode,
                    QuestionKind::Choice => LaunchQuestionKindView::Choice,
                },
                asked: q.asked,
                prompt: q.prompt.clone(),
                default_option: q.default_option.clone(),
                options: q
                    .options
                    .iter()
                    .map(|o| LaunchOptionView {
                        id: o.id.clone(),
                        label: o.label.clone(),
                        hint: o.hint.clone(),
                    })
                    .collect(),
            })
            .collect(),
    };
    Ok(FocusLaunchConfig {
        focus_id: display.focus_id,
        display_name: display.display_name,
        description: display.description,
        ui_one_liner: display.ui_one_liner,
        questions,
    })
}

// ---------------------------------------------------------------------------
// get_launch_state
// ---------------------------------------------------------------------------

#[tauri::command]
#[specta::specta]
pub async fn get_launch_state(
    persona_id: String,
    context_key: String,
    focus_id: String,
    key_registry: State<'_, KeyRegistry>,
    pool: State<'_, sqlx::SqlitePool>,
) -> Result<LaunchState, String> {
    let _lease = crate::auth::writer_gate::command(&key_registry).await?;
    let (user_id, key_hex) = session_user_and_key(&key_registry).await?;
    get_launch_state_for_session(
        &pool,
        &user_id,
        &key_hex,
        &persona_id,
        &context_key,
        &focus_id,
    )
    .await
}

pub(crate) async fn get_launch_state_for_session(
    pool: &sqlx::SqlitePool,
    user_id: &str,
    key_hex: &str,
    persona_id: &str,
    context_key: &str,
    focus_id: &str,
) -> Result<LaunchState, String> {
    require_persona(pool, user_id, persona_id).await?;
    require_focus_id(focus_id)?;
    require_context_key(context_key)?;
    let config = read_config(pool, focus_id).await?;
    let mut conn = output_store::open_outputs_db_retrying(user_id, persona_id, key_hex)
        .await
        .map_err(|e| unavailable("open outputs.db", e))?;
    state_from(&mut conn, config.as_ref(), context_key, focus_id).await
}

async fn state_from(
    conn: &mut sqlx::SqliteConnection,
    config: Option<&LaunchConfig>,
    context_key: &str,
    focus_id: &str,
) -> Result<LaunchState, String> {
    // A latch that cannot be read is an error here too: the screen must not say
    // "saved" about a chat whose mode it does not know.
    let one_time_chat = matches!(
        session_store::write_mode(conn, context_key)
            .await
            .map_err(|e| unavailable("read the one-time latch", e))?,
        session_store::WriteMode::Suppressed
    );
    let stored = launch_answer_store::read_answers(conn, context_key, focus_id)
        .await
        .map_err(|e| unavailable("read the stored answers", e))?;

    let mut questions = Vec::new();
    for q in config.map(|c| c.questions.as_slice()).unwrap_or(&[]) {
        let answer = if q.asked {
            stored.get(q.slot.as_str()).cloned()
        } else {
            None
        };
        let stale = answer.as_deref().is_some_and(|v| q.option(v).is_none());
        let valid = answer.as_deref().filter(|v| q.option(v).is_some());
        questions.push(LaunchQuestionState {
            question_id: q.id.clone(),
            effective: valid.unwrap_or(&q.default_option).to_owned(),
            from_default: valid.is_none(),
            answer: answer.clone(),
            stale,
        });
    }
    // A stored per-run change that fails its validator is a store problem, never
    // shown.
    let run_change = match stored.get(bound_spec::RUN_CHANGE) {
        None => None,
        Some(v) => {
            let ok = bound_spec::spec(bound_spec::RUN_CHANGE).is_some_and(|s| (s.validate)(v));
            if !ok {
                return Err(unavailable("run_change", "invalid stored value"));
            }
            Some(v.clone())
        }
    };
    let defaults_apply = one_time_chat && stored.is_empty();
    Ok(LaunchState {
        focus_id: focus_id.to_owned(),
        questions,
        run_change,
        one_time_chat,
        defaults_apply,
    })
}

// ---------------------------------------------------------------------------
// submit_launch_answers
// ---------------------------------------------------------------------------

#[tauri::command]
#[specta::specta]
pub async fn submit_launch_answers(
    persona_id: String,
    context_key: String,
    focus_id: String,
    answers: Vec<LaunchAnswerInput>,
    run_change: Option<String>,
    key_registry: State<'_, KeyRegistry>,
    pool: State<'_, sqlx::SqlitePool>,
) -> Result<LaunchState, String> {
    let _lease = crate::auth::writer_gate::command(&key_registry).await?;
    let (user_id, key_hex) = session_user_and_key(&key_registry).await?;
    submit_launch_answers_for_session(
        &pool,
        &user_id,
        &key_hex,
        &persona_id,
        &context_key,
        &focus_id,
        &answers,
        run_change.as_deref(),
    )
    .await
}

// 8 arguments: the pool, the session's user and key, the three ids the command
// takes, and the answers. Bundling them would hide which come from the session.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn submit_launch_answers_for_session(
    pool: &sqlx::SqlitePool,
    user_id: &str,
    key_hex: &str,
    persona_id: &str,
    context_key: &str,
    focus_id: &str,
    answers: &[LaunchAnswerInput],
    run_change: Option<&str>,
) -> Result<LaunchState, String> {
    require_persona(pool, user_id, persona_id).await?;
    require_focus_id(focus_id)?;
    require_context_key(context_key)?;
    let reg = registry().map_err(|e| unavailable("document types", e))?;
    let config = read_config(pool, focus_id)
        .await?
        .ok_or_else(|| "no_launch_config".to_owned())?;
    let pairs: Vec<(&str, &str)> = answers
        .iter()
        .map(|a| (a.question_id.as_str(), a.value.as_str()))
        .collect();
    let validated = launch_config::validate_submission(&config, &pairs, run_change, reg)
        .map_err(|e| e.code().to_owned())?;
    let mut conn = output_store::open_outputs_db_retrying(user_id, persona_id, key_hex)
        .await
        .map_err(|e| unavailable("open outputs.db", e))?;
    launch_answer_store::put_answers(&mut conn, context_key, focus_id, &validated)
        .await
        .map_err(|e| match e {
            PutError::OneTimeChat => "one_time_chat".to_owned(),
            PutError::LatchUnreadable(m) => unavailable("read the one-time latch", m),
            PutError::Store(m) => unavailable("save the answers", m),
        })?;
    state_from(&mut conn, Some(&config), context_key, focus_id).await
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::registry::key_hex;
    use crate::test_support::ENV_MUTEX;

    const CHAT: &str = "chat-launch";
    const FOCUS: &str = "launch-fixture";
    const MASTER_KEY: [u8; crate::auth::kdf::MASTER_KEY_LEN] =
        [0xABu8; crate::auth::kdf::MASTER_KEY_LEN];

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

    async fn add_user(pool: &sqlx::SqlitePool, name: &str) -> String {
        let id = uuid::Uuid::new_v4().to_string();
        crate::auth::user_store::create_user(
            pool,
            &id,
            name,
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
        id
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
        let user = add_user(&pool, "Launch User").await;
        let persona = uuid::Uuid::new_v4().to_string();
        persona_store::create_persona(&pool, &persona, "P", "personal", &user, &key, None)
            .await
            .expect("create_persona");
        // A loadable Focus with one local step, a mode question (quick by default,
        // careful) and a choice on launch_answer_1, plus one preset (not asked).
        sqlx::query(
            "INSERT INTO focuses
                (focus_id, display_name, description, version, max_routing_tier,
                 output_type, suggest_in_focuses, multi_source_validation,
                 generic_title_template, status, created_at, updated_at)
             VALUES (?, 'Launch Fixture', 'A fixture Focus.', '1.0', 'local_only', 'general',
                     '[]', 0, 'Hidden item', 'shipped', datetime('now'), datetime('now'))",
        )
        .bind(FOCUS)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO focus_block_compositions
                (id, focus_id, action_id, block_stable_id, sequence_index, created_at, updated_at)
             VALUES ('lf:a', ?, 'a', NULL, 0, datetime('now'), datetime('now'))",
        )
        .bind(FOCUS)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO focus_block_customization
                (id, composition_id, persona_id, customization, created_at, updated_at)
             VALUES ('lf:a:d', 'lf:a', NULL, ?, datetime('now'), datetime('now'))",
        )
        .bind(
            r#"{"routing_tier": 1, "task_type": "quick_response", "output_var": "o",
                "prompt_template": "Say hello."}"#,
        )
        .execute(&pool)
        .await
        .unwrap();
        for sql in [
            "INSERT INTO focus_launch_questions
               (focus_id, question_id, slot, kind, sort_order, asked, default_option, prompt)
             VALUES ('launch-fixture', 'how', 'launch_mode', 'mode', 0, 1, 'quick', 'How?'),
                    ('launch-fixture', 'thorough', 'launch_answer_1', 'choice', 1, 1, 'simple', 'How thorough?'),
                    ('launch-fixture', 'kind', 'document_type', 'choice', 2, 0, 'rma_letter', 'Which kind?')",
            "INSERT INTO focus_launch_options
               (focus_id, question_id, option_id, sort_order, label, hint)
             VALUES ('launch-fixture', 'how', 'quick', 0, 'Quick', ''),
                    ('launch-fixture', 'how', 'careful', 1, 'Careful', 'Slower'),
                    ('launch-fixture', 'thorough', 'simple', 0, 'Simple', ''),
                    ('launch-fixture', 'thorough', 'polished', 1, 'Polished', ''),
                    ('launch-fixture', 'kind', 'rma_letter', 0, 'RMA letter', '')",
            "INSERT INTO focus_mode_profiles (focus_id, option_id, envelope)
             VALUES ('launch-fixture', 'quick', '{\"highest_tier\":\"local_only\"}'),
                    ('launch-fixture', 'careful', '{}')",
        ] {
            sqlx::query(sql).execute(&pool).await.unwrap();
        }
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

    fn a(q: &str, v: &str) -> LaunchAnswerInput {
        LaunchAnswerInput {
            question_id: q.to_owned(),
            value: v.to_owned(),
        }
    }

    async fn state(e: &Env, chat: &str) -> Result<LaunchState, String> {
        get_launch_state_for_session(&e.pool, &e.user, &e.key, &e.persona, chat, FOCUS).await
    }

    async fn submit(
        e: &Env,
        chat: &str,
        answers: &[LaunchAnswerInput],
        run_change: Option<&str>,
    ) -> Result<LaunchState, String> {
        submit_launch_answers_for_session(
            &e.pool, &e.user, &e.key, &e.persona, chat, FOCUS, answers, run_change,
        )
        .await
    }

    async fn latch(e: &Env, chat: &str) {
        let mut conn = output_store::open_outputs_db(&e.user, &e.persona, &e.key)
            .await
            .unwrap();
        session_store::latch_write_suppressed(&mut conn, chat, FOCUS, "run-1")
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn the_config_is_shipped_data_with_the_display_fields_and_no_mode_profile() {
        let e = env().await;
        let c = get_focus_launch_config_for_session(&e.pool, FOCUS)
            .await
            .unwrap();
        assert_eq!(c.display_name, "Launch Fixture");
        assert_eq!(c.description, "A fixture Focus.");
        assert_eq!(c.ui_one_liner, None);
        assert_eq!(c.questions.len(), 3);
        let how = &c.questions[0];
        assert_eq!(how.kind, LaunchQuestionKindView::Mode);
        assert_eq!(how.default_option, "quick");
        assert_eq!(
            how.options
                .iter()
                .map(|o| o.id.as_str())
                .collect::<Vec<_>>(),
            ["quick", "careful"]
        );
        assert!(!c.questions[2].asked, "the preset is not asked");
        // The serialised config carries no envelope, tier or destination.
        let json = serde_json::to_string(&serde_json::json!({
            "q": c.questions.iter().map(|q| format!("{q:?}")).collect::<Vec<_>>()
        }))
        .unwrap();
        for banned in ["highest_tier", "destinations", "envelope", "local_only"] {
            assert!(!json.contains(banned), "{banned}");
        }
        // A shipped Focus with no question: its display data and an empty list.
        let ra = get_focus_launch_config_for_session(&e.pool, "role-assessment")
            .await
            .unwrap();
        assert!(ra.ui_one_liner.is_some());
        assert!(ra.questions.is_empty());
        // items.id=903: the Writing Assistant ships its two questions, and no profile.
        let wa = get_focus_launch_config_for_session(&e.pool, "writing-assistant")
            .await
            .unwrap();
        assert_eq!(wa.display_name, "Writing Assistant");
        let ids: Vec<&str> = wa.questions.iter().map(|q| q.id.as_str()).collect();
        assert_eq!(ids, ["mode", "kind"]);
        assert_eq!(wa.questions[0].kind, LaunchQuestionKindView::Mode);
        assert_eq!(wa.questions[0].default_option, "medium");
        assert_eq!(wa.questions[1].default_option, "personal_note");
        // An unknown or malformed Focus id.
        assert_eq!(
            get_focus_launch_config_for_session(&e.pool, "no-such-focus")
                .await
                .unwrap_err(),
            "invalid_focus_id"
        );
        assert_eq!(
            get_focus_launch_config_for_session(&e.pool, "Bad Id")
                .await
                .unwrap_err(),
            "invalid_focus_id"
        );
    }

    #[tokio::test]
    async fn a_fresh_chat_shows_every_default_and_a_submit_stores_and_returns_the_state() {
        let e = env().await;
        let s = state(&e, CHAT).await.unwrap();
        assert_eq!(s.questions.len(), 3);
        for q in &s.questions {
            assert!(q.from_default && q.answer.is_none() && !q.stale, "{q:?}");
        }
        assert_eq!(s.questions[0].effective, "quick");
        assert!(!s.one_time_chat && !s.defaults_apply && s.run_change.is_none());

        let s = submit(
            &e,
            CHAT,
            &[a("how", "careful"), a("thorough", "polished")],
            Some(r#"{"tone":"firm"}"#),
        )
        .await
        .unwrap();
        assert_eq!(s.questions[0].answer.as_deref(), Some("careful"));
        assert_eq!(s.questions[0].effective, "careful");
        assert!(!s.questions[0].from_default);
        assert_eq!(s.questions[1].effective, "polished");
        assert_eq!(s.run_change.as_deref(), Some(r#"{"tone":"firm"}"#));
        // A read returns the same, and another chat is untouched.
        assert_eq!(state(&e, CHAT).await.unwrap(), s);
        assert!(state(&e, "other-chat").await.unwrap().questions[0].from_default);
    }

    #[tokio::test]
    async fn a_latched_chat_refuses_the_submit_with_its_own_code_and_still_reads() {
        let e = env().await;
        // An answer stored before the latch stays and still applies.
        submit(&e, CHAT, &[a("how", "careful")], None)
            .await
            .unwrap();
        latch(&e, CHAT).await;
        let s = state(&e, CHAT).await.unwrap();
        assert!(s.one_time_chat);
        assert!(!s.defaults_apply, "a pre-latch answer applies");
        assert_eq!(s.questions[0].effective, "careful");
        assert_eq!(
            submit(&e, CHAT, &[a("how", "quick")], None)
                .await
                .unwrap_err(),
            "one_time_chat"
        );
        assert_eq!(
            state(&e, CHAT).await.unwrap().questions[0].effective,
            "careful"
        );
        // A latched chat that never answered: defaults apply, and a submit is refused.
        latch(&e, "fresh-latched").await;
        let s = state(&e, "fresh-latched").await.unwrap();
        assert!(s.one_time_chat && s.defaults_apply);
        assert_eq!(s.questions[0].effective, "quick");
        assert_eq!(
            submit(&e, "fresh-latched", &[a("how", "careful")], None)
                .await
                .unwrap_err(),
            "one_time_chat"
        );
    }

    #[tokio::test]
    async fn every_refusal_has_its_own_documented_code() {
        let e = env().await;
        let other_user = add_user(&e.pool, "Someone Else").await;
        let mut results: Vec<Result<LaunchState, String>> = Vec::new();
        // persona: not a UUID, a non-canonical UUID, a UUID the user is not in.
        for p in [
            "not-a-uuid".to_owned(),
            e.persona.to_uppercase(),
            uuid::Uuid::new_v4().to_string(),
        ] {
            results.push(
                submit_launch_answers_for_session(
                    &e.pool,
                    &e.user,
                    &e.key,
                    &p,
                    CHAT,
                    FOCUS,
                    &[],
                    None,
                )
                .await,
            );
        }
        results.push(
            submit_launch_answers_for_session(
                &e.pool,
                &other_user,
                &e.key,
                &e.persona,
                CHAT,
                FOCUS,
                &[],
                None,
            )
            .await,
        );
        for bad in ["", "Bad Id", &"a".repeat(65)] {
            results.push(
                submit_launch_answers_for_session(
                    &e.pool,
                    &e.user,
                    &e.key,
                    &e.persona,
                    CHAT,
                    bad,
                    &[],
                    None,
                )
                .await,
            );
        }
        for bad in ["", &"c".repeat(129)] {
            results.push(
                submit_launch_answers_for_session(
                    &e.pool,
                    &e.user,
                    &e.key,
                    &e.persona,
                    bad,
                    FOCUS,
                    &[],
                    None,
                )
                .await,
            );
        }
        // A Focus with no launch config.
        results.push(
            submit_launch_answers_for_session(
                &e.pool,
                &e.user,
                &e.key,
                &e.persona,
                CHAT,
                "role-assessment",
                &[],
                None,
            )
            .await,
        );
        results.push(submit(&e, CHAT, &[a("nope", "quick")], None).await);
        results.push(submit(&e, CHAT, &[a("kind", "rma_letter")], None).await);
        results.push(submit(&e, CHAT, &[a("how", "slow")], None).await);
        results.push(submit(&e, CHAT, &[a("how", "quick"), a("how", "careful")], None).await);
        results.push(submit(&e, CHAT, &[], Some(r#"{"highest_tier":"unrestricted"}"#)).await);
        results.push(submit(&e, CHAT, &[], Some(r#"{"tone":"sparkly"}"#)).await);
        let expect = [
            "persona_not_found",
            "persona_not_found",
            "persona_not_found",
            "persona_not_found",
            "invalid_focus_id",
            "invalid_focus_id",
            "invalid_focus_id",
            "invalid_context_key",
            "invalid_context_key",
            "no_launch_config",
            "unknown_question",
            "question_not_asked",
            "invalid_answer",
            "invalid_answer",
            "invalid_run_change",
            "invalid_run_change",
        ];
        assert_eq!(results.len(), expect.len());
        for (r, want) in results.iter().zip(expect) {
            let err = r.as_ref().unwrap_err();
            assert_eq!(err, want);
            assert!(ERROR_CODES.contains(&err.as_str()));
        }
        // Nothing was written by any refused submit.
        assert!(state(&e, CHAT).await.unwrap().questions[0].from_default);
        // The read has the same input checks.
        assert_eq!(
            get_launch_state_for_session(&e.pool, &e.user, &e.key, "nope", CHAT, FOCUS)
                .await
                .unwrap_err(),
            "persona_not_found"
        );
    }

    #[tokio::test]
    async fn a_stale_stored_answer_is_flagged_and_a_bad_stored_run_change_is_unavailable() {
        let e = env().await;
        let mut conn = output_store::open_outputs_db(&e.user, &e.persona, &e.key)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO session_variables
               (context_key, focus_id, name, value, version, written_by_run, created_at, updated_at)
             VALUES (?, ?, 'launch_mode', 'gone', 1, 'r', 'now', 'now')",
        )
        .bind(CHAT)
        .bind(FOCUS)
        .execute(&mut conn)
        .await
        .unwrap();
        let s = state(&e, CHAT).await.unwrap();
        assert!(s.questions[0].stale && s.questions[0].from_default);
        assert_eq!(s.questions[0].effective, "quick");
        // Submitting a valid answer replaces it.
        let s = submit(&e, CHAT, &[a("how", "careful")], None)
            .await
            .unwrap();
        assert!(!s.questions[0].stale);
        sqlx::query(
            "INSERT INTO session_variables
               (context_key, focus_id, name, value, version, written_by_run, created_at, updated_at)
             VALUES (?, ?, 'run_change', 'not json', 1, 'r', 'now', 'now')",
        )
        .bind(CHAT)
        .bind(FOCUS)
        .execute(&mut conn)
        .await
        .unwrap();
        assert_eq!(state(&e, CHAT).await.unwrap_err(), "launch_unavailable");
    }

    /// Every error code has a placeholder key in the frontend's en.json (Chat-BRAND
    /// owns the wording), and no key without a code.
    #[test]
    fn every_error_code_has_a_placeholder_key_in_en_json() {
        let json: serde_json::Value =
            serde_json::from_str(include_str!("../../../frontend/src/i18n/en.json")).unwrap();
        for code in ERROR_CODES {
            let text = json["launch"]["error"][code]
                .as_str()
                .unwrap_or_else(|| panic!("en.json has no launch.error.{code}"));
            assert!(!text.trim().is_empty(), "{code}");
        }
        let keys: Vec<&String> = json["launch"]["error"]
            .as_object()
            .unwrap()
            .keys()
            .collect();
        assert_eq!(keys.len(), ERROR_CODES.len(), "{keys:?}");
    }

    /// The module takes no ceiling, tier or destination, and approves nothing: its
    /// production code names no spend gate, provider, plan or approval (decisions.id=
    /// 942 Q3), and opens the outputs database only with the retry.
    #[test]
    fn this_module_approves_nothing_and_never_names_a_spend_or_provider_path() {
        let src = include_str!("launch.rs");
        let body = src.split("#[cfg(test)]").next().unwrap();
        for banned in [
            "spend::",
            "provider_selection",
            "providers::",
            "PlanRequired",
            "approve",
            "consent_decision",
            "ExternalAccess",
            "max_permitted_tier",
        ] {
            // The header comment states what this module never does: scan code only.
            let code: String = body
                .lines()
                .filter(|l| !l.trim_start().starts_with("//"))
                .collect::<Vec<_>>()
                .join("\n");
            assert!(!code.contains(banned), "launch.rs names '{banned}'");
        }
        assert!(!body.contains("output_store::open_outputs_db("));
        assert!(body.contains("output_store::open_outputs_db_retrying("));
    }
}
