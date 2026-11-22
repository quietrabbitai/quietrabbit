// src-tauri/src/commands/document_session_tests.rs
//
// items.id=903 unit B: set_document_privacy_flag through its *_for_session body, against
// a real shared.db and the persona's encrypted outputs.db under a scratch data root.

use super::*;
use crate::auth::registry::key_hex;
use crate::persistence::session_store::latch_write_suppressed;
use crate::test_support::ENV_MUTEX;

const CHAT: &str = "chat-doc";
const FOCUS: &str = "doc-fixture";
const PLAIN: &str = "doc-plain";
const MASTER_KEY: [u8; crate::auth::kdf::MASTER_KEY_LEN] =
    [0xCDu8; crate::auth::kdf::MASTER_KEY_LEN];

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
    .expect("create_user");
    let persona = uuid::Uuid::new_v4().to_string();
    persona_store::create_persona(&pool, &persona, "P", "personal", &user, &key, None)
        .await
        .expect("create_persona");
    for focus in [FOCUS, PLAIN] {
        sqlx::query(
            "INSERT INTO focuses
                (focus_id, display_name, description, version, max_routing_tier,
                 output_type, suggest_in_focuses, multi_source_validation,
                 generic_title_template, status, created_at, updated_at)
             VALUES (?, 'Doc Fixture', '', '1.0', 'local_only', 'general', '[]', 0,
                     'Hidden', 'shipped', datetime('now'), datetime('now'))",
        )
        .bind(focus)
        .execute(&pool)
        .await
        .unwrap();
    }
    for sql in [
        "INSERT INTO focus_launch_questions
           (focus_id, question_id, slot, kind, sort_order, asked, default_option, prompt)
         VALUES ('doc-fixture', 'kind', 'document_type', 'choice', 0, 1, 'cover_letter', 'Which kind?')",
        "INSERT INTO focus_launch_options
           (focus_id, question_id, option_id, sort_order, label, hint)
         VALUES ('doc-fixture', 'kind', 'cover_letter', 0, 'Cover letter', ''),
                ('doc-fixture', 'kind', 'rma_letter', 1, 'RMA letter', '')",
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

async fn set(
    e: &Env,
    chat: &str,
    focus: &str,
    flag: DocumentPrivacyFlag,
    output: Option<&str>,
) -> Result<DocumentSessionView, String> {
    set_document_privacy_flag_for_session(
        &e.pool, &e.user, &e.key, &e.persona, chat, focus, flag, output,
    )
    .await
}

async fn conn(e: &Env) -> sqlx::SqliteConnection {
    output_store::open_outputs_db_retrying(&e.user, &e.persona, &e.key)
        .await
        .unwrap()
}

#[tokio::test]
async fn the_flag_is_stored_with_the_runs_document_type() {
    let e = env().await;
    let v = set(&e, CHAT, FOCUS, DocumentPrivacyFlag::Medical, None)
        .await
        .unwrap();
    assert_eq!(v.document_type, "cover_letter", "the question's default");
    assert_eq!(v.document_privacy_flag, Some(DocumentPrivacyFlag::Medical));
    assert!(v.local_only);
    // A stored answer is the type, as the run start would select it.
    let mut c = conn(&e).await;
    sqlx::query(
        "INSERT INTO session_variables
            (context_key, focus_id, name, value, version, written_by_run, created_at, updated_at)
         VALUES (?, ?, 'document_type', 'rma_letter', 1, 'launch', 't', 't')",
    )
    .bind(CHAT)
    .bind(FOCUS)
    .execute(&mut c)
    .await
    .unwrap();
    let v = set(&e, CHAT, FOCUS, DocumentPrivacyFlag::General, None)
        .await
        .unwrap();
    assert_eq!(v.document_type, "rma_letter");
    assert!(!v.local_only);
}

#[tokio::test]
async fn a_focus_without_a_document_type_question_is_refused() {
    let e = env().await;
    assert_eq!(
        set(&e, CHAT, PLAIN, DocumentPrivacyFlag::Legal, None)
            .await
            .unwrap_err(),
        "no_document_type"
    );
}

#[tokio::test]
async fn a_one_time_chat_is_refused_with_its_own_code() {
    let e = env().await;
    let mut c = conn(&e).await;
    latch_write_suppressed(&mut c, CHAT, FOCUS, "run-1")
        .await
        .unwrap();
    assert_eq!(
        set(&e, CHAT, FOCUS, DocumentPrivacyFlag::Legal, None)
            .await
            .unwrap_err(),
        "one_time_chat"
    );
    assert!(document_session_store::get(&mut c, CHAT, FOCUS)
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn a_foreign_output_id_refuses_the_whole_command() {
    let e = env().await;
    let mut c = conn(&e).await;
    for (run, chat, out) in [("r1", CHAT, "o1"), ("r2", "chat-other", "o2")] {
        sqlx::query(
            "INSERT INTO focus_runs (id, focus_id, status, started_at, context_key)
             VALUES (?, ?, 'complete', datetime('now'), ?)",
        )
        .bind(run)
        .bind(FOCUS)
        .bind(chat)
        .execute(&mut c)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO outputs (id, focus_run_id, output_type, content, created_at, updated_at)
             VALUES (?, ?, 'document', 'x', datetime('now'), datetime('now'))",
        )
        .bind(out)
        .bind(run)
        .execute(&mut c)
        .await
        .unwrap();
    }
    assert_eq!(
        set(&e, CHAT, FOCUS, DocumentPrivacyFlag::Legal, Some("o2"))
            .await
            .unwrap_err(),
        "invalid_output_id"
    );
    assert!(
        document_session_store::get(&mut c, CHAT, FOCUS)
            .await
            .unwrap()
            .is_none(),
        "nothing was written"
    );
    set(&e, CHAT, FOCUS, DocumentPrivacyFlag::Legal, Some("o1"))
        .await
        .unwrap();
    let f: Option<String> =
        sqlx::query_scalar("SELECT document_privacy_flag FROM outputs WHERE id = 'o1'")
            .fetch_one(&mut c)
            .await
            .unwrap();
    assert_eq!(f.as_deref(), Some("legal"));
}

#[tokio::test]
async fn bad_ids_are_refused_before_any_read() {
    let e = env().await;
    let foreign = uuid::Uuid::new_v4().to_string();
    let r = set_document_privacy_flag_for_session(
        &e.pool,
        &e.user,
        &e.key,
        &foreign,
        CHAT,
        FOCUS,
        DocumentPrivacyFlag::Legal,
        None,
    )
    .await;
    assert_eq!(r.unwrap_err(), "persona_not_found");
    assert_eq!(
        set(&e, CHAT, "Bad Focus", DocumentPrivacyFlag::Legal, None)
            .await
            .unwrap_err(),
        "invalid_focus_id"
    );
    assert_eq!(
        set(&e, "", FOCUS, DocumentPrivacyFlag::Legal, None)
            .await
            .unwrap_err(),
        "invalid_context_key"
    );
}

#[test]
fn every_error_code_has_a_placeholder_key_in_en_json() {
    let json: serde_json::Value =
        serde_json::from_str(include_str!("../../../frontend/src/i18n/en.json")).unwrap();
    for code in ERROR_CODES {
        let text = json["document"]["error"][code]
            .as_str()
            .unwrap_or_else(|| panic!("en.json has no document.error.{code}"));
        assert!(!text.trim().is_empty(), "{code}");
    }
    assert_eq!(
        json["document"]["error"].as_object().unwrap().len(),
        ERROR_CODES.len()
    );
}

/// A flag only labels and tightens: the module names no spend gate, provider, plan,
/// approval, focus_settings writer or raise.
#[test]
fn the_command_raises_nothing_and_approves_nothing() {
    let src = include_str!("document_session.rs");
    let prod = src.split("#[cfg(test)]").next().unwrap();
    let code: String = prod
        .lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n");
    for banned in [
        "spend::",
        "provider",
        "plan_approval",
        "run_plan",
        "focus_settings",
        "set_raise",
        "raised_ceiling",
    ] {
        assert!(!code.contains(banned), "document_session.rs names {banned}");
    }
}
