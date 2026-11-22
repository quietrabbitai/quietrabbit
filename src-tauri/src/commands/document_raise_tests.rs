// src-tauri/src/commands/document_raise_tests.rs
//
// items.id=903 unit D: the one-document ceiling raise through its *_for_session bodies,
// against a real shared.db, the persona's encrypted outputs.db and personal.db under a
// scratch data root, and the shipped Writing Assistant (its launch questions and its
// tier-manifest ceiling, anonymous_required).

use std::time::{Duration, Instant};

use super::*;
use crate::auth::registry::key_hex;
use crate::persistence::document_session_store::DocumentPrivacyFlag;
use crate::persistence::session_store::latch_write_suppressed;
use crate::test_support::ENV_MUTEX;

const CHAT: &str = "chat-raise";
const FOCUS: &str = "writing-assistant";
const MASTER_KEY: [u8; crate::auth::kdf::MASTER_KEY_LEN] =
    [0x5Au8; crate::auth::kdf::MASTER_KEY_LEN];

struct Env {
    _tempdir: tempfile::TempDir,
    _lock: tokio::sync::MutexGuard<'static, ()>,
    saved_root: Option<String>,
    pool: sqlx::SqlitePool,
    user: String,
    persona: String,
    key: String,
    challenges: RaiseChallenges,
}

impl Drop for Env {
    fn drop(&mut self) {
        match &self.saved_root {
            Some(v) => std::env::set_var("QR_DATA_ROOT", v),
            None => std::env::remove_var("QR_DATA_ROOT"),
        }
    }
}

async fn env_with(persona_type: &str) -> Env {
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
        "Raise User",
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
    persona_store::create_persona(&pool, &persona, "P", persona_type, &user, &key, None)
        .await
        .expect("create_persona");
    // The user keeps the Writing Assistant local in focus_settings: a raise is above it.
    set_settings(&pool, &persona, "local_only", "open").await;
    Env {
        _tempdir: tempdir,
        _lock: lock,
        saved_root,
        pool,
        user,
        persona,
        key,
        challenges: RaiseChallenges::default(),
    }
}

async fn env() -> Env {
    env_with("standard").await
}

async fn set_settings(pool: &sqlx::SqlitePool, persona: &str, tier: &str, profile: &str) {
    let n = sqlx::query(
        "UPDATE focus_settings SET max_permitted_tier = ?, focus_profile = ?
         WHERE persona_id = ? AND focus_id = ?",
    )
    .bind(tier)
    .bind(profile)
    .bind(persona)
    .bind(FOCUS)
    .execute(pool)
    .await
    .unwrap()
    .rows_affected();
    if n == 0 {
        focus_settings_store::create_focus_settings(
            pool,
            persona,
            FOCUS,
            "bidirectional",
            "shared",
            2,
            tier.parse().unwrap(),
            profile,
            None,
        )
        .await
        .unwrap();
    }
}

impl Env {
    fn ids(&self) -> Ids<'_> {
        Ids {
            user_id: &self.user,
            key_hex: &self.key,
            persona_id: &self.persona,
            context_key: CHAT,
            focus_id: FOCUS,
        }
    }

    async fn flag(&self, flag: DocumentPrivacyFlag) {
        crate::commands::document_session::set_document_privacy_flag_for_session(
            &self.pool,
            &self.user,
            &self.key,
            &self.persona,
            CHAT,
            FOCUS,
            flag,
            None,
        )
        .await
        .unwrap();
    }

    async fn request(
        &self,
        target: ExternalAccess,
        now: Instant,
    ) -> Result<RaiseChallengeView, String> {
        request_for_session(&self.pool, &self.challenges, &self.ids(), target, now).await
    }

    async fn confirm(
        &self,
        token: &str,
        ack: bool,
        now: Instant,
    ) -> Result<DocumentRaiseView, String> {
        confirm_for_session(
            &self.pool,
            &self.challenges,
            &self.user,
            &self.key,
            token,
            ack,
            now,
        )
        .await
    }

    async fn session(&self) -> Option<DocumentSession> {
        let mut c = output_store::open_outputs_db_retrying(&self.user, &self.persona, &self.key)
            .await
            .unwrap();
        document_session_store::get(&mut c, CHAT, FOCUS)
            .await
            .unwrap()
    }

    async fn audits(&self) -> Vec<consent_audit_store::ConsentAuditRecord> {
        let mut p = open_personal_db(&self.user, &self.persona, &self.key)
            .await
            .unwrap();
        consent_audit_store::list(&mut p, FOCUS).await.unwrap()
    }
}

#[tokio::test]
async fn a_raise_is_requested_waited_for_confirmed_once_and_audited_first() {
    let e = env().await;
    e.flag(DocumentPrivacyFlag::General).await;
    let t0 = Instant::now();
    let c = e
        .request(ExternalAccess::AnonymousRequired, t0)
        .await
        .unwrap();
    assert_eq!(c.document_type, "personal_note");
    assert_eq!(c.target, ExternalAccess::AnonymousRequired);
    assert_eq!((c.wait_ms, c.expires_in_ms), (5_000, 600_000));
    assert_eq!(c.previous_raise_at, None);
    // Friction: too early, then not acknowledged; neither consumes the token.
    assert_eq!(
        e.confirm(&c.token, true, t0 + Duration::from_secs(4))
            .await
            .unwrap_err(),
        "too_early"
    );
    assert_eq!(
        e.confirm(&c.token, false, t0 + Duration::from_secs(6))
            .await
            .unwrap_err(),
        "not_acknowledged"
    );
    assert!(e.audits().await.is_empty(), "nothing written yet");
    assert_eq!(e.session().await.unwrap().raised_ceiling, None);
    let done = e
        .confirm(&c.token, true, t0 + Duration::from_secs(6))
        .await
        .unwrap();
    assert_eq!(done.raised_ceiling, ExternalAccess::AnonymousRequired);
    // The audit row, and the raise that names it.
    let audits = e.audits().await;
    assert_eq!(audits.len(), 1);
    let a = &audits[0];
    assert_eq!(a.event_type, ConsentEventType::DocumentCeilingRaised);
    assert_eq!(a.document_type.as_deref(), Some("personal_note"));
    assert_eq!(a.content_summary, RAISE_AUDIT_SUMMARY);
    assert_eq!(a.text_version, Some(RAISE_TEXT_VERSION));
    assert_eq!(a.consent_reference_id, None);
    let s = e.session().await.unwrap();
    assert_eq!(s.raised_ceiling, Some(ExternalAccess::AnonymousRequired));
    assert_eq!(s.consent_audit_id.as_deref(), Some(a.id.as_str()));
    // Single use.
    assert_eq!(
        e.confirm(&c.token, true, t0 + Duration::from_secs(7))
            .await
            .unwrap_err(),
        "invalid_token"
    );
    // The same raise again is not a raise; focus_settings is untouched.
    assert_eq!(
        e.request(ExternalAccess::AnonymousRequired, t0)
            .await
            .unwrap_err(),
        "not_a_raise"
    );
    let fs = focus_settings_store::get_focus_settings(&e.pool, &e.persona, FOCUS)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(fs.max_permitted_tier, ExternalAccess::LocalOnly);
}

#[tokio::test]
async fn each_refusal_has_its_own_code_and_writes_nothing() {
    let e = env().await;
    let now = Instant::now();
    // No document yet.
    assert_eq!(
        e.request(ExternalAccess::AnonymousRequired, now)
            .await
            .unwrap_err(),
        "no_document"
    );
    e.flag(DocumentPrivacyFlag::Legal).await;
    // Above the Focus's own ceiling (the tier manifest holds it at anonymous_required).
    assert_eq!(
        e.request(ExternalAccess::Unrestricted, now)
            .await
            .unwrap_err(),
        "above_focus_ceiling"
    );
    assert_eq!(
        e.request(ExternalAccess::LocalOnly, now).await.unwrap_err(),
        "not_a_raise"
    );
    // A protected Focus profile.
    set_settings(&e.pool, &e.persona, "local_only", "protected").await;
    assert_eq!(
        e.request(ExternalAccess::AnonymousRequired, now)
            .await
            .unwrap_err(),
        "protected_persona"
    );
    set_settings(&e.pool, &e.persona, "local_only", "open").await;
    // A latched chat.
    let mut c = output_store::open_outputs_db_retrying(&e.user, &e.persona, &e.key)
        .await
        .unwrap();
    latch_write_suppressed(&mut c, CHAT, FOCUS, "run-1")
        .await
        .unwrap();
    assert_eq!(
        e.request(ExternalAccess::AnonymousRequired, now)
            .await
            .unwrap_err(),
        "one_time_chat"
    );
    assert!(e.audits().await.is_empty());
    assert_eq!(e.session().await.unwrap().raised_ceiling, None);
    // Bad ids.
    let mut ids = e.ids();
    let foreign = uuid::Uuid::new_v4().to_string();
    ids.persona_id = &foreign;
    assert_eq!(
        request_for_session(
            &e.pool,
            &e.challenges,
            &ids,
            ExternalAccess::AnonymousRequired,
            now
        )
        .await
        .unwrap_err(),
        "persona_not_found"
    );
}

#[tokio::test]
async fn a_protected_persona_is_refused() {
    let e = env_with("protected").await;
    e.flag(DocumentPrivacyFlag::General).await;
    assert_eq!(
        e.request(ExternalAccess::AnonymousRequired, Instant::now())
            .await
            .unwrap_err(),
        "protected_persona"
    );
}

#[tokio::test]
async fn a_state_change_after_the_request_is_caught_at_confirm() {
    let e = env().await;
    e.flag(DocumentPrivacyFlag::General).await;
    let t0 = Instant::now();
    let c = e
        .request(ExternalAccess::AnonymousRequired, t0)
        .await
        .unwrap();
    set_settings(&e.pool, &e.persona, "local_only", "protected").await;
    assert_eq!(
        e.confirm(&c.token, true, t0 + MIN_WAIT).await.unwrap_err(),
        "protected_persona"
    );
    assert!(e.audits().await.is_empty());
}

/// A writer whose second write fails: the audit row stands, the ceiling is unraised.
struct FailingRaise;

impl RaiseWriter for FailingRaise {
    async fn audit(
        &mut self,
        ids: &Ids<'_>,
        document_type: &str,
        previous: Option<&str>,
    ) -> Result<String, String> {
        StoreRaiseWriter.audit(ids, document_type, previous).await
    }
    async fn raise(
        &mut self,
        _: &Ids<'_>,
        _: &str,
        _: ExternalAccess,
        _: &str,
    ) -> Result<DocumentSession, String> {
        Err("raise_unavailable".to_owned())
    }
}

/// A writer whose first write fails: nothing is raised.
struct FailingAudit {
    raised: bool,
}

impl RaiseWriter for FailingAudit {
    async fn audit(&mut self, _: &Ids<'_>, _: &str, _: Option<&str>) -> Result<String, String> {
        Err("raise_unavailable".to_owned())
    }
    async fn raise(
        &mut self,
        ids: &Ids<'_>,
        document_type: &str,
        target: ExternalAccess,
        audit_id: &str,
    ) -> Result<DocumentSession, String> {
        self.raised = true;
        StoreRaiseWriter
            .raise(ids, document_type, target, audit_id)
            .await
    }
}

#[tokio::test]
async fn the_audit_row_comes_first_and_a_fault_never_leaves_an_unaudited_raise() {
    let e = env().await;
    e.flag(DocumentPrivacyFlag::General).await;
    let t0 = Instant::now();
    let c = e
        .request(ExternalAccess::AnonymousRequired, t0)
        .await
        .unwrap();
    let mut audit_fails = FailingAudit { raised: false };
    let r = confirm_with(
        &e.pool,
        &e.challenges,
        &e.user,
        &e.key,
        &c.token,
        true,
        t0 + MIN_WAIT,
        &mut audit_fails,
    )
    .await;
    assert_eq!(r.unwrap_err(), "raise_unavailable");
    assert!(!audit_fails.raised, "no raise without its audit row");
    assert_eq!(e.session().await.unwrap().raised_ceiling, None);

    let c = e
        .request(ExternalAccess::AnonymousRequired, t0)
        .await
        .unwrap();
    let r = confirm_with(
        &e.pool,
        &e.challenges,
        &e.user,
        &e.key,
        &c.token,
        true,
        t0 + MIN_WAIT,
        &mut FailingRaise,
    )
    .await;
    assert_eq!(r.unwrap_err(), "raise_unavailable");
    assert_eq!(e.audits().await.len(), 1, "the consent given is recorded");
    assert_eq!(
        e.session().await.unwrap().raised_ceiling,
        None,
        "but nothing was raised"
    );
}

#[tokio::test]
async fn a_repeat_raise_shows_and_chains_the_earlier_one() {
    let e = env().await;
    e.flag(DocumentPrivacyFlag::General).await;
    let t0 = Instant::now();
    let c = e
        .request(ExternalAccess::AnonymousRequired, t0)
        .await
        .unwrap();
    e.confirm(&c.token, true, t0 + MIN_WAIT).await.unwrap();
    let first = e.audits().await[0].clone();
    // A changed document type voids the raise; changing back starts a new document.
    let mut conn = output_store::open_outputs_db_retrying(&e.user, &e.persona, &e.key)
        .await
        .unwrap();
    document_session_store::set_privacy_flag(
        &mut conn,
        CHAT,
        FOCUS,
        "rma_letter",
        DocumentPrivacyFlag::General,
    )
    .await
    .unwrap();
    e.flag(DocumentPrivacyFlag::General).await;
    assert_eq!(e.session().await.unwrap().raised_ceiling, None);
    let c = e
        .request(ExternalAccess::AnonymousRequired, t0)
        .await
        .unwrap();
    assert_eq!(
        c.previous_raise_at.as_deref(),
        Some(first.created_at.as_str())
    );
    e.confirm(&c.token, true, t0 + MIN_WAIT).await.unwrap();
    let audits = e.audits().await;
    assert_eq!(audits.len(), 2);
    assert_eq!(
        audits[1].consent_reference_id.as_deref(),
        Some(first.id.as_str())
    );
}

#[test]
fn every_error_code_has_a_placeholder_key_in_en_json() {
    let json: serde_json::Value =
        serde_json::from_str(include_str!("../../../frontend/src/i18n/en.json")).unwrap();
    for code in ERROR_CODES {
        let text = json["document"]["raise_error"][code]
            .as_str()
            .unwrap_or_else(|| panic!("en.json has no document.raise_error.{code}"));
        assert!(!text.trim().is_empty(), "{code}");
    }
    assert_eq!(
        json["document"]["raise_error"].as_object().unwrap().len(),
        ERROR_CODES.len()
    );
}

fn rust_files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
    for e in std::fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.is_dir() {
            rust_files(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
}

fn production(text: &str) -> String {
    text.split("\n#[cfg(test)]")
        .next()
        .unwrap()
        .lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Only these commands write a raise; and nothing that selects, approves, spends or
/// gates a step names the raise (decisions.id=942 Q3: a raise approves nothing).
#[test]
fn only_the_raise_commands_write_a_raise_and_nothing_downstream_reads_it() {
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    rust_files(&src, &mut files);
    let mut writers = Vec::new();
    for f in &files {
        let rel = f.strip_prefix(&src).unwrap().to_string_lossy().to_string();
        if rel.ends_with("tests.rs") || rel == "persistence/document_session_store.rs" {
            continue;
        }
        let code = production(&std::fs::read_to_string(f).unwrap());
        if code.contains("set_raise(") {
            writers.push(rel.clone());
        }
        if [
            "conductor/run_plan.rs",
            "conductor/plan_gate.rs",
            "conductor/plan_approval.rs",
            "conductor/executor.rs",
            "conductor/step_treatment.rs",
            "conductor/model_resolver.rs",
            "conductor/model_resolver_input.rs",
            "conductor/store_writer.rs",
            "conductor/store_op.rs",
        ]
        .contains(&rel.as_str())
            || rel.starts_with("spend/")
        {
            for banned in [
                "raised_ceiling",
                "effective_ceiling",
                "document_raise",
                "set_raise",
            ] {
                assert!(!code.contains(banned), "{rel} names {banned}");
            }
        }
    }
    assert_eq!(writers, ["commands/document_raise.rs"]);
}
