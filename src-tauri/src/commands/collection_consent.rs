// src-tauri/src/commands/collection_consent.rs
//
// items.id=920 stage 2 (decisions.id=944, decisions.id=509): the user's own
// commands for the ONE informed consent a focus_level Focus (Tech Support) needs
// before it saves anything. They are the only callers of
// focus_settings_store::set_collection_consent and of
// consent_audit_store::append (a source scan in conductor/store_op_tests.rs pins
// that), and no model path reaches them: a decision is a Tauri command the
// frontend calls from the user's own confirm.
//
//   get_collection_consent_state(persona_id, focus_id)
//       the state, the stored text version, the CURRENT text version, whether a
//       decision is needed, and the promise ids the text cites.
//   submit_collection_consent_decision(persona_id, focus_id, decision, text_version)
//       decision granted or declined, with the text version the user was SHOWN. A
//       version that is not the current one is rejected (the text changed under
//       them: they are asked again).
//   withdraw_collection_consent(persona_id, focus_id)
//   list_consent_audit_events(persona_id, focus_id)   read only
//
// EVERY command: takes writer_gate::command first (a key rotation waits for it),
// derives the user and the data key from the unlocked session through
// persona::session_user_and_key (no user_id argument exists to spoof), checks the
// Persona id is a UUID and belongs to that user (require_persona, private and
// written so items.id=638 can lift it), and does its work in a *_for_session body
// with no Tauri state. None can take an AppHandle (the note in commands/consent.rs
// about untestable AppHandle commands), so each is tested through its body.
//
// WRITE ORDER, fail-safe in both directions (the gate opens only on the flag, the
// flag is the last thing a grant writes and the first thing a withdrawal writes):
//   GRANT     audit event FIRST, then the flag. A crash between them leaves the
//             audit event and collection OFF; the retry reuses the event and sets
//             the flag.
//   WITHDRAW  the flag FIRST (so collection stops at once), then the audit event.
//             A failure between them leaves collection OFF with no withdrawal
//             event: the command returns an error so the frontend retries, and the
//             retry appends the missing event. It never reports success without it.
// A decline records the state and the version shown through
// set_collection_consent (state declined) and appends no event of its own, EXCEPT
// that it closes an open chain: if the newest collection_consent_* event is still
// a grant it appends collection_consent_withdrawn referencing it (checked on every
// decline call, between the state write and the event write, with withdraw's
// retry semantics). A withdrawal sets
// 'withdrawn', appends collection_consent_withdrawn referencing the grant, KEEPS
// whatever the Focus already collected, and deletes nothing: removal is the
// user's own, separate act. A later grant appends a new granted event referencing
// the withdrawal.
//
// Error strings are short codes for the frontend (persona_not_found,
// invalid_focus_id, focus_settings_not_found, consent_text_stale,
// consent_text_unavailable, consent_granted_use_withdraw, no_consent_to_withdraw).
// content_summary of an event is one of the fixed strings below, never content.

use serde::{Deserialize, Serialize};
use specta::Type;
use tauri::State;

use crate::auth::registry::KeyRegistry;
use crate::commands::persona::session_user_and_key;
use crate::conductor::collection_consent_text as consent_text;
use crate::persistence::consent_audit_store::{
    self as audit, ConsentAuditRecord, ConsentEventType, NewConsentEvent,
};
use crate::persistence::focus_settings_store::{self as settings, CollectionConsent};
use crate::persistence::persona_store;
use crate::persistence::personal_store::open_personal_db;

const SUMMARY_GRANTED: &str = "Collection consent granted for this Focus.";
const SUMMARY_WITHDRAWN: &str = "Collection consent withdrawn for this Focus.";

// ---------------------------------------------------------------------------
// IPC types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct CollectionConsentState {
    pub consent: CollectionConsent,
    /// The text version the stored decision was given for (0 when none).
    pub stored_version: i32,
    /// The text version in force now.
    pub current_text_version: i32,
    /// True when the user should be asked: not_asked, or granted or declined for
    /// a text version other than the current one. A withdrawn user is not asked
    /// again, and a declined user at the current version is not asked again.
    pub needs_decision: bool,
    /// The promise ids the current text cites. Prose is i18n copy keyed by id.
    pub promises: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum CollectionConsentDecision {
    Granted,
    Declined,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct ConsentAuditEvent {
    pub id: String,
    pub event_type: String,
    pub text_version: Option<i32>,
    pub consent_reference_id: Option<String>,
    pub content_summary: String,
    pub document_type: Option<String>,
    pub created_at: String,
}

// ---------------------------------------------------------------------------
// Validation helpers
// ---------------------------------------------------------------------------

/// The Persona id must be a canonical UUID AND belong to the signed-in user. The
/// two failures answer alike (persona_not_found): nothing is learned about
/// another account's Personas. Private on purpose, so items.id=638 can lift it
/// into one shared check without a caller outside this module to move.
pub(crate) async fn require_persona(
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
        Err(e) => Err(e.to_string()),
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

/// The current text version, or an error when the bundled data is unusable.
fn current_version() -> Result<i64, String> {
    match consent_text::current_version() {
        v if v >= 1 => Ok(v),
        _ => Err("consent_text_unavailable".to_owned()),
    }
}

async fn read_state(
    pool: &sqlx::SqlitePool,
    persona_id: &str,
    focus_id: &str,
) -> Result<settings::CollectionConsentState, String> {
    settings::get_collection_consent(pool, persona_id, focus_id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "focus_settings_not_found".to_owned())
}

/// Whether the user must be asked, for a stored state and the current version.
fn needs_decision(state: &settings::CollectionConsentState, current: i64) -> bool {
    match state.consent {
        CollectionConsent::NotAsked => true,
        CollectionConsent::Granted | CollectionConsent::Declined => state.version != current,
        CollectionConsent::Withdrawn => false,
    }
}

fn to_view(state: &settings::CollectionConsentState) -> Result<CollectionConsentState, String> {
    let current = current_version()?;
    let promises = consent_text::registry()?.promises.clone();
    Ok(CollectionConsentState {
        consent: state.consent,
        stored_version: i32::try_from(state.version).map_err(|e| e.to_string())?,
        current_text_version: i32::try_from(current).map_err(|e| e.to_string())?,
        needs_decision: needs_decision(state, current),
        promises,
    })
}

/// The newest collection_consent_* event of the Focus (granted or withdrawn).
async fn latest_collection_event(
    conn: &mut sqlx::SqliteConnection,
    focus_id: &str,
) -> Result<Option<ConsentAuditRecord>, String> {
    let all = audit::list(conn, focus_id)
        .await
        .map_err(|e| e.to_string())?;
    Ok(all.into_iter().rev().find(|r| {
        matches!(
            r.event_type,
            ConsentEventType::CollectionConsentGranted
                | ConsentEventType::CollectionConsentWithdrawn
        )
    }))
}

fn event_view(r: ConsentAuditRecord) -> ConsentAuditEvent {
    ConsentAuditEvent {
        id: r.id,
        event_type: r.event_type.as_str().to_owned(),
        text_version: r.text_version.and_then(|v| i32::try_from(v).ok()),
        consent_reference_id: r.consent_reference_id,
        content_summary: r.content_summary,
        document_type: r.document_type,
        created_at: r.created_at,
    }
}

/// Append collection_consent_withdrawn referencing `after` (the grant, or
/// whatever event is newest), with that event's text version.
async fn append_withdrawn(
    conn: &mut sqlx::SqliteConnection,
    focus_id: &str,
    after: Option<&ConsentAuditRecord>,
) -> Result<(), String> {
    audit::append(
        conn,
        &NewConsentEvent {
            event_type: ConsentEventType::CollectionConsentWithdrawn,
            focus_slug: focus_id,
            run_id: None,
            text_version: after.and_then(|r| r.text_version),
            consent_reference_id: after.map(|r| r.id.as_str()),
            content_summary: SUMMARY_WITHDRAWN,
            document_type: None,
        },
    )
    .await
    .map(|_| ())
    .map_err(|e| format!("consent_audit_failed: {e}"))
}

/// A step run between the two writes of a decision. The commands pass a no-op;
/// a test passes one that fails, to prove each order is safe.
pub(crate) type Between<'a> = &'a (dyn Fn() -> Result<(), String> + Send + Sync);

fn no_hook() -> Result<(), String> {
    Ok(())
}

// ---------------------------------------------------------------------------
// Bodies (no Tauri state)
// ---------------------------------------------------------------------------

pub(crate) async fn get_state_for_session(
    pool: &sqlx::SqlitePool,
    user_id: &str,
    persona_id: &str,
    focus_id: &str,
) -> Result<CollectionConsentState, String> {
    require_persona(pool, user_id, persona_id).await?;
    require_focus_id(focus_id)?;
    to_view(&read_state(pool, persona_id, focus_id).await?)
}

#[allow(clippy::too_many_arguments)] // The session's ids, the key and the hook; one body.
pub(crate) async fn submit_decision_for_session(
    pool: &sqlx::SqlitePool,
    user_id: &str,
    key_hex: &str,
    persona_id: &str,
    focus_id: &str,
    decision: CollectionConsentDecision,
    text_version: i64,
    between: Between<'_>,
) -> Result<CollectionConsentState, String> {
    require_persona(pool, user_id, persona_id).await?;
    require_focus_id(focus_id)?;
    // The user decided about a text. It must be the one in force.
    if text_version != current_version()? {
        return Err("consent_text_stale".to_owned());
    }
    let state = read_state(pool, persona_id, focus_id).await?;

    match decision {
        CollectionConsentDecision::Granted => {
            // Idempotent: a current grant is already in place, and a repeated
            // click appends nothing.
            if state.is_current_grant() {
                return to_view(&state);
            }
            let mut conn = open_personal_db(user_id, persona_id, key_hex)
                .await
                .map_err(|e| e.to_string())?;
            let latest = latest_collection_event(&mut conn, focus_id).await?;
            // A retry after a crash between the two writes: the event for this
            // very grant is already the newest, so it is reused, not repeated.
            let already_recorded = latest.as_ref().is_some_and(|r| {
                r.event_type == ConsentEventType::CollectionConsentGranted
                    && r.text_version == Some(text_version)
            });
            if !already_recorded {
                audit::append(
                    &mut conn,
                    &NewConsentEvent {
                        event_type: ConsentEventType::CollectionConsentGranted,
                        focus_slug: focus_id,
                        run_id: None,
                        text_version: Some(text_version),
                        consent_reference_id: latest.as_ref().map(|r| r.id.as_str()),
                        content_summary: SUMMARY_GRANTED,
                        document_type: None,
                    },
                )
                .await
                .map_err(|e| e.to_string())?;
            }
            // AUDIT FIRST, FLAG SECOND: a failure from here on leaves collection
            // off.
            between()?;
            settings::set_collection_consent(
                pool,
                persona_id,
                focus_id,
                CollectionConsent::Granted,
                text_version,
            )
            .await
            .map_err(|e| e.to_string())?;
        }
        CollectionConsentDecision::Declined => {
            // Turning collection OFF after a grant is a withdrawal, and a
            // withdrawal has its own audited chain.
            if state.is_current_grant() {
                return Err("consent_granted_use_withdraw".to_owned());
            }
            let mut conn = open_personal_db(user_id, persona_id, key_hex)
                .await
                .map_err(|e| e.to_string())?;
            // The state and the version shown.
            settings::set_collection_consent(
                pool,
                persona_id,
                focus_id,
                CollectionConsent::Declined,
                text_version,
            )
            .await
            .map_err(|e| e.to_string())?;
            between()?;
            // CHAIN CLOSE, on EVERY decline call (so a retry after a failed append
            // reaches it): a decline appends no event of its own, but if the newest
            // collection event is still a GRANT (a grant for an older text, or an
            // orphan left by a crash between the two grant writes), the decline
            // ends it, so the chain never shows a grant that is no longer in
            // force. A newest withdrawal, or no event at all, appends nothing.
            // Errors keep withdraw's semantics: collection is already off, so the
            // frontend retries.
            let latest = latest_collection_event(&mut conn, focus_id).await?;
            if let Some(grant) = latest
                .as_ref()
                .filter(|r| r.event_type == ConsentEventType::CollectionConsentGranted)
            {
                append_withdrawn(&mut conn, focus_id, Some(grant)).await?;
            }
        }
    }
    to_view(&read_state(pool, persona_id, focus_id).await?)
}

pub(crate) async fn withdraw_for_session(
    pool: &sqlx::SqlitePool,
    user_id: &str,
    key_hex: &str,
    persona_id: &str,
    focus_id: &str,
    between: Between<'_>,
) -> Result<CollectionConsentState, String> {
    require_persona(pool, user_id, persona_id).await?;
    require_focus_id(focus_id)?;
    let state = read_state(pool, persona_id, focus_id).await?;
    // Withdrawn is allowed too: that is the retry of a withdrawal whose audit
    // event did not get written.
    if !matches!(
        state.consent,
        CollectionConsent::Granted | CollectionConsent::Withdrawn
    ) {
        return Err("no_consent_to_withdraw".to_owned());
    }
    let mut conn = open_personal_db(user_id, persona_id, key_hex)
        .await
        .map_err(|e| e.to_string())?;

    // FLAG FIRST: collection stops now. Nothing already collected is touched.
    if state.consent == CollectionConsent::Granted {
        settings::set_collection_consent(
            pool,
            persona_id,
            focus_id,
            CollectionConsent::Withdrawn,
            0,
        )
        .await
        .map_err(|e| e.to_string())?;
    }
    between()?;

    // AUDIT SECOND. An error here leaves collection off and tells the frontend
    // to retry: the state is withdrawn, so the retry lands here again.
    let latest = latest_collection_event(&mut conn, focus_id).await?;
    let already_recorded = latest
        .as_ref()
        .is_some_and(|r| r.event_type == ConsentEventType::CollectionConsentWithdrawn);
    if !already_recorded {
        append_withdrawn(&mut conn, focus_id, latest.as_ref()).await?;
    }
    to_view(&read_state(pool, persona_id, focus_id).await?)
}

pub(crate) async fn list_events_for_session(
    pool: &sqlx::SqlitePool,
    user_id: &str,
    key_hex: &str,
    persona_id: &str,
    focus_id: &str,
) -> Result<Vec<ConsentAuditEvent>, String> {
    require_persona(pool, user_id, persona_id).await?;
    require_focus_id(focus_id)?;
    let mut conn = open_personal_db(user_id, persona_id, key_hex)
        .await
        .map_err(|e| e.to_string())?;
    let rows = audit::list(&mut conn, focus_id)
        .await
        .map_err(|e| e.to_string())?;
    Ok(rows.into_iter().map(event_view).collect())
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

#[tauri::command]
#[specta::specta]
pub async fn get_collection_consent_state(
    persona_id: String,
    focus_id: String,
    key_registry: State<'_, KeyRegistry>,
    pool: State<'_, sqlx::SqlitePool>,
) -> Result<CollectionConsentState, String> {
    let _lease = crate::auth::writer_gate::command(&key_registry).await?;
    let (user_id, _key_hex) = session_user_and_key(&key_registry).await?;
    get_state_for_session(&pool, &user_id, &persona_id, &focus_id).await
}

/// `text_version` is the version of the text the user was SHOWN. A version that
/// is not the current one is rejected with consent_text_stale.
#[tauri::command]
#[specta::specta]
pub async fn submit_collection_consent_decision(
    persona_id: String,
    focus_id: String,
    decision: CollectionConsentDecision,
    text_version: i32,
    key_registry: State<'_, KeyRegistry>,
    pool: State<'_, sqlx::SqlitePool>,
) -> Result<CollectionConsentState, String> {
    let _lease = crate::auth::writer_gate::command(&key_registry).await?;
    let (user_id, key_hex) = session_user_and_key(&key_registry).await?;
    submit_decision_for_session(
        &pool,
        &user_id,
        &key_hex,
        &persona_id,
        &focus_id,
        decision,
        i64::from(text_version),
        &no_hook,
    )
    .await
}

/// Stops collection at once and records the withdrawal. What the Focus already
/// collected stays until the user deletes it. An Err means the audit event was
/// not written (collection is already off): call it again.
#[tauri::command]
#[specta::specta]
pub async fn withdraw_collection_consent(
    persona_id: String,
    focus_id: String,
    key_registry: State<'_, KeyRegistry>,
    pool: State<'_, sqlx::SqlitePool>,
) -> Result<CollectionConsentState, String> {
    let _lease = crate::auth::writer_gate::command(&key_registry).await?;
    let (user_id, key_hex) = session_user_and_key(&key_registry).await?;
    withdraw_for_session(&pool, &user_id, &key_hex, &persona_id, &focus_id, &no_hook).await
}

#[tauri::command]
#[specta::specta]
pub async fn list_consent_audit_events(
    persona_id: String,
    focus_id: String,
    key_registry: State<'_, KeyRegistry>,
    pool: State<'_, sqlx::SqlitePool>,
) -> Result<Vec<ConsentAuditEvent>, String> {
    let _lease = crate::auth::writer_gate::command(&key_registry).await?;
    let (user_id, key_hex) = session_user_and_key(&key_registry).await?;
    list_events_for_session(&pool, &user_id, &key_hex, &persona_id, &focus_id).await
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------
//
// UUID fixtures throughout (require_persona refuses anything else; the older
// consent tests use a non-UUID id). Real shared.db, real encrypted personal.db
// and outputs.db files under a scratch QR_DATA_ROOT.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::registry::key_hex;
    use crate::conductor::tokens::ExternalAccess;
    use crate::persistence::persona_store;
    use crate::test_support::{mock_app_with_registry, populate_registry, ENV_MUTEX};
    use tauri::Manager;

    const FOCUS: &str = "tech-support";
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

    async fn add_persona(pool: &sqlx::SqlitePool, user: &str, key: &str) -> String {
        let id = uuid::Uuid::new_v4().to_string();
        persona_store::create_persona(pool, &id, "P", "personal", user, key, None)
            .await
            .expect("create_persona");
        settings::create_focus_settings(
            pool,
            &id,
            FOCUS,
            "bidirectional",
            "shared",
            2,
            ExternalAccess::LocalOnly,
            "open",
            None,
        )
        .await
        .expect("focus_settings row");
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
        let user = add_user(&pool, "Consent User").await;
        let persona = add_persona(&pool, &user, &key).await;
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

    fn cv() -> i64 {
        consent_text::current_version()
    }

    async fn grant(e: &Env, version: i64) -> Result<CollectionConsentState, String> {
        submit_decision_for_session(
            &e.pool,
            &e.user,
            &e.key,
            &e.persona,
            FOCUS,
            CollectionConsentDecision::Granted,
            version,
            &no_hook,
        )
        .await
    }

    async fn decline(e: &Env, version: i64) -> Result<CollectionConsentState, String> {
        submit_decision_for_session(
            &e.pool,
            &e.user,
            &e.key,
            &e.persona,
            FOCUS,
            CollectionConsentDecision::Declined,
            version,
            &no_hook,
        )
        .await
    }

    async fn withdraw(e: &Env) -> Result<CollectionConsentState, String> {
        withdraw_for_session(&e.pool, &e.user, &e.key, &e.persona, FOCUS, &no_hook).await
    }

    async fn events(e: &Env) -> Vec<ConsentAuditEvent> {
        list_events_for_session(&e.pool, &e.user, &e.key, &e.persona, FOCUS)
            .await
            .unwrap()
    }

    async fn state(e: &Env) -> settings::CollectionConsentState {
        settings::get_collection_consent(&e.pool, &e.persona, FOCUS)
            .await
            .unwrap()
            .unwrap()
    }

    fn fail() -> Result<(), String> {
        Err("injected failure".to_owned())
    }

    // -- decisions ----------------------------------------------------------

    #[tokio::test]
    async fn a_new_persona_needs_a_decision_and_the_state_names_the_current_text() {
        let e = env().await;
        let s = get_state_for_session(&e.pool, &e.user, &e.persona, FOCUS)
            .await
            .unwrap();
        assert_eq!(s.consent, CollectionConsent::NotAsked);
        assert_eq!(s.stored_version, 0);
        assert_eq!(i64::from(s.current_text_version), cv());
        assert!(s.needs_decision);
        assert_eq!(s.promises, consent_text::registry().unwrap().promises);
    }

    #[test]
    fn who_is_asked_again_follows_the_decision_table() {
        let st = |consent, version| settings::CollectionConsentState { consent, version };
        use CollectionConsent::*;
        // (state, stored version, current version) -> asked?
        for (c, v, cur, asked) in [
            (NotAsked, 0, 1, true),
            (Granted, 1, 1, false),
            (Granted, 1, 2, true),
            (Declined, 1, 1, false),
            (Declined, 1, 2, true),
            (Withdrawn, 0, 1, false),
            (Withdrawn, 0, 2, false),
        ] {
            assert_eq!(
                needs_decision(&st(c, v), cur),
                asked,
                "{c:?} v{v} now v{cur}"
            );
        }
    }

    #[tokio::test]
    async fn a_grant_appends_the_event_then_opens_the_gate_and_is_idempotent() {
        let e = env().await;
        let s = grant(&e, cv()).await.unwrap();
        assert_eq!(s.consent, CollectionConsent::Granted);
        assert!(!s.needs_decision);
        assert!(state(&e).await.is_current_grant());
        let ev = events(&e).await;
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0].event_type, "collection_consent_granted");
        assert_eq!(i64::from(ev[0].text_version.unwrap()), cv());
        assert_eq!(ev[0].consent_reference_id, None);
        assert_eq!(ev[0].content_summary, SUMMARY_GRANTED);

        // A repeated confirm changes nothing and appends nothing.
        let again = grant(&e, cv()).await.unwrap();
        assert_eq!(again, s);
        assert_eq!(events(&e).await.len(), 1);
    }

    #[tokio::test]
    async fn a_stale_text_version_is_rejected_and_nothing_is_written() {
        let e = env().await;
        for bad in [cv() + 1, cv() - 1, 0, -3] {
            assert_eq!(grant(&e, bad).await.unwrap_err(), "consent_text_stale");
            assert_eq!(decline(&e, bad).await.unwrap_err(), "consent_text_stale");
        }
        assert_eq!(state(&e).await.consent, CollectionConsent::NotAsked);
        assert!(events(&e).await.is_empty());
    }

    #[tokio::test]
    async fn a_decline_records_state_and_version_and_no_event_and_is_not_asked_again() {
        let e = env().await;
        let s = decline(&e, cv()).await.unwrap();
        assert_eq!(s.consent, CollectionConsent::Declined);
        assert_eq!(i64::from(s.stored_version), cv());
        assert!(!s.needs_decision, "a declined user at the current version");
        assert!(events(&e).await.is_empty(), "a decline appends no event");
        assert!(!state(&e).await.is_current_grant(), "and opens nothing");
        // Idempotent.
        assert_eq!(decline(&e, cv()).await.unwrap(), s);

        // A decline given for an older text is asked again.
        settings::set_collection_consent(
            &e.pool,
            &e.persona,
            FOCUS,
            CollectionConsent::Declined,
            99,
        )
        .await
        .unwrap();
        let s = get_state_for_session(&e.pool, &e.user, &e.persona, FOCUS)
            .await
            .unwrap();
        assert!(s.needs_decision);

        // A user who declined can still grant later.
        assert_eq!(
            grant(&e, cv()).await.unwrap().consent,
            CollectionConsent::Granted
        );
    }

    #[tokio::test]
    async fn a_grant_of_an_older_text_asks_again_and_a_new_grant_references_the_old() {
        let e = env().await;
        // A grant given for another text version (the text has changed since):
        // its event and its flag, as a grant at that version would have left them.
        let other = cv() + 100;
        let mut conn = open_personal_db(&e.user, &e.persona, &e.key).await.unwrap();
        let first = audit::append(
            &mut conn,
            &NewConsentEvent {
                event_type: ConsentEventType::CollectionConsentGranted,
                focus_slug: FOCUS,
                run_id: None,
                text_version: Some(other),
                consent_reference_id: None,
                content_summary: SUMMARY_GRANTED,
                document_type: None,
            },
        )
        .await
        .unwrap();
        settings::set_collection_consent(
            &e.pool,
            &e.persona,
            FOCUS,
            CollectionConsent::Granted,
            other,
        )
        .await
        .unwrap();
        let s = get_state_for_session(&e.pool, &e.user, &e.persona, FOCUS)
            .await
            .unwrap();
        assert!(
            s.needs_decision,
            "an older text version forces a new decision"
        );
        assert!(
            !state(&e).await.is_current_grant(),
            "and the gate is closed"
        );

        // The re-consent is the same event type pointing at the prior event.
        grant(&e, cv()).await.unwrap();
        let ev = events(&e).await;
        assert_eq!(ev.len(), 2);
        assert_eq!(ev[1].event_type, "collection_consent_granted");
        assert_eq!(ev[1].consent_reference_id.as_deref(), Some(first.as_str()));
        assert_eq!(i64::from(ev[1].text_version.unwrap()), cv());
        assert!(state(&e).await.is_current_grant());
    }

    #[tokio::test]
    async fn declining_over_a_current_grant_is_refused_it_is_a_withdrawal() {
        let e = env().await;
        grant(&e, cv()).await.unwrap();
        assert_eq!(
            decline(&e, cv()).await.unwrap_err(),
            "consent_granted_use_withdraw"
        );
        assert!(state(&e).await.is_current_grant());
    }

    // -- a decline closes an open grant chain --------------------------------

    async fn decline_with(
        e: &Env,
        version: i64,
        between: Between<'_>,
    ) -> Result<CollectionConsentState, String> {
        submit_decision_for_session(
            &e.pool,
            &e.user,
            &e.key,
            &e.persona,
            FOCUS,
            CollectionConsentDecision::Declined,
            version,
            between,
        )
        .await
    }

    async fn append_granted(e: &Env, version: i64) -> String {
        let mut conn = open_personal_db(&e.user, &e.persona, &e.key).await.unwrap();
        audit::append(
            &mut conn,
            &NewConsentEvent {
                event_type: ConsentEventType::CollectionConsentGranted,
                focus_slug: FOCUS,
                run_id: None,
                text_version: Some(version),
                consent_reference_id: None,
                content_summary: SUMMARY_GRANTED,
                document_type: None,
            },
        )
        .await
        .unwrap()
    }

    /// Grant at one text, the text changes, the user declines the new one: the
    /// chain ends in a withdrawal that references the grant.
    #[tokio::test]
    async fn declining_a_new_text_after_a_grant_closes_the_chain_with_a_withdrawal() {
        let e = env().await;
        let old = cv() + 100;
        let first = append_granted(&e, old).await;
        settings::set_collection_consent(
            &e.pool,
            &e.persona,
            FOCUS,
            CollectionConsent::Granted,
            old,
        )
        .await
        .unwrap();
        let s = decline(&e, cv()).await.unwrap();
        assert_eq!(s.consent, CollectionConsent::Declined);
        assert!(!s.needs_decision);
        let ev = events(&e).await;
        assert_eq!(ev.len(), 2);
        assert_eq!(ev[1].event_type, "collection_consent_withdrawn");
        assert_eq!(ev[1].consent_reference_id.as_deref(), Some(first.as_str()));
        assert_eq!(
            i64::from(ev[1].text_version.unwrap()),
            old,
            "the grant's text"
        );
        assert_eq!(ev[1].content_summary, SUMMARY_WITHDRAWN);
        // Declining again appends nothing: the newest event is now a withdrawal.
        decline(&e, cv()).await.unwrap();
        assert_eq!(events(&e).await.len(), 2);
    }

    /// An orphan grant event (a crash between the two grant writes) followed by a
    /// decline: the orphan is closed by a withdrawal.
    #[tokio::test]
    async fn an_orphan_grant_event_is_closed_by_a_later_decline() {
        let e = env().await;
        let err = submit_decision_for_session(
            &e.pool,
            &e.user,
            &e.key,
            &e.persona,
            FOCUS,
            CollectionConsentDecision::Granted,
            cv(),
            &fail,
        )
        .await
        .unwrap_err();
        assert_eq!(err, "injected failure");
        assert_eq!(state(&e).await.consent, CollectionConsent::NotAsked);
        let orphan = events(&e).await[0].id.clone();

        decline(&e, cv()).await.unwrap();
        let ev = events(&e).await;
        assert_eq!(ev.len(), 2);
        assert_eq!(ev[1].event_type, "collection_consent_withdrawn");
        assert_eq!(ev[1].consent_reference_id.as_deref(), Some(orphan.as_str()));
    }

    /// A plain decline, no events at all or a newest withdrawal, appends nothing.
    #[tokio::test]
    async fn a_plain_decline_appends_nothing() {
        let e = env().await;
        decline(&e, cv()).await.unwrap();
        assert!(events(&e).await.is_empty());

        // After a withdrawal: grant, withdraw, then a decline appends nothing more.
        grant(&e, cv()).await.unwrap();
        withdraw(&e).await.unwrap();
        assert_eq!(events(&e).await.len(), 2);
        decline(&e, cv()).await.unwrap();
        assert_eq!(events(&e).await.len(), 2, "the newest was a withdrawal");
        assert_eq!(state(&e).await.consent, CollectionConsent::Declined);
    }

    /// A failure between the state write and the event write leaves the decline
    /// recorded and collection off; the retry appends the withdrawal exactly once.
    #[tokio::test]
    async fn a_failure_between_a_declines_state_write_and_its_event_is_retried_once() {
        let e = env().await;
        let old = cv() + 100;
        let first = append_granted(&e, old).await;
        settings::set_collection_consent(
            &e.pool,
            &e.persona,
            FOCUS,
            CollectionConsent::Granted,
            old,
        )
        .await
        .unwrap();

        let err = decline_with(&e, cv(), &fail).await.unwrap_err();
        assert_eq!(err, "injected failure");
        assert_eq!(state(&e).await.consent, CollectionConsent::Declined);
        assert!(!state(&e).await.is_current_grant());
        assert_eq!(events(&e).await.len(), 1, "no withdrawal yet");

        // The retry reaches the check again and appends exactly once.
        decline(&e, cv()).await.unwrap();
        let ev = events(&e).await;
        assert_eq!(ev.len(), 2);
        assert_eq!(ev[1].event_type, "collection_consent_withdrawn");
        assert_eq!(ev[1].consent_reference_id.as_deref(), Some(first.as_str()));
        decline(&e, cv()).await.unwrap();
        assert_eq!(events(&e).await.len(), 2, "and not again");
    }

    // -- fail-safe write order ----------------------------------------------

    /// GRANT: the audit event is written first, so a failure between the two
    /// writes leaves the event and collection OFF; the retry reuses the event.
    #[tokio::test]
    async fn a_failure_between_the_two_writes_of_a_grant_leaves_collection_off() {
        let e = env().await;
        let err = submit_decision_for_session(
            &e.pool,
            &e.user,
            &e.key,
            &e.persona,
            FOCUS,
            CollectionConsentDecision::Granted,
            cv(),
            &fail,
        )
        .await
        .unwrap_err();
        assert_eq!(err, "injected failure");
        let st = state(&e).await;
        assert_eq!(st.consent, CollectionConsent::NotAsked);
        assert!(!st.is_current_grant(), "collection is off");
        let ev = events(&e).await;
        assert_eq!(ev.len(), 1, "the audit event was written first");
        assert_eq!(ev[0].event_type, "collection_consent_granted");

        // The retry completes the grant WITHOUT a second event.
        let s = grant(&e, cv()).await.unwrap();
        assert_eq!(s.consent, CollectionConsent::Granted);
        assert!(state(&e).await.is_current_grant());
        assert_eq!(events(&e).await.len(), 1);
    }

    /// WITHDRAW: the flag is cleared first, so a failure between the two writes
    /// leaves collection OFF with no withdrawal event, and the command errors so
    /// the frontend retries; the retry appends the missing event, once.
    #[tokio::test]
    async fn a_failure_between_the_two_writes_of_a_withdrawal_stops_collection_and_the_retry_records_it(
    ) {
        let e = env().await;
        grant(&e, cv()).await.unwrap();
        let err = withdraw_for_session(&e.pool, &e.user, &e.key, &e.persona, FOCUS, &fail)
            .await
            .unwrap_err();
        assert_eq!(err, "injected failure");
        assert_eq!(state(&e).await.consent, CollectionConsent::Withdrawn);
        assert!(
            !state(&e).await.is_current_grant(),
            "collection is already off"
        );
        let ev = events(&e).await;
        assert_eq!(ev.len(), 1, "no withdrawal event yet");
        assert_eq!(ev[0].event_type, "collection_consent_granted");

        // Retry: the state is withdrawn, the missing event is appended.
        let s = withdraw(&e).await.unwrap();
        assert_eq!(s.consent, CollectionConsent::Withdrawn);
        let ev = events(&e).await;
        assert_eq!(ev.len(), 2);
        assert_eq!(ev[1].event_type, "collection_consent_withdrawn");
        assert_eq!(
            ev[1].consent_reference_id.as_deref(),
            Some(ev[0].id.as_str())
        );
        // And a further call is a no-op.
        withdraw(&e).await.unwrap();
        assert_eq!(events(&e).await.len(), 2);
    }

    // -- the withdrawal chain -----------------------------------------------

    #[tokio::test]
    async fn the_chain_grant_withdraw_grant_references_each_prior_event_and_keeps_data() {
        use crate::persistence::session_store::{
            get_session_variable, put_session_variable, SessionWrite,
        };
        let e = env().await;
        grant(&e, cv()).await.unwrap();

        // Something the Focus collected.
        let mut out =
            crate::persistence::output_store::open_outputs_db(&e.user, &e.persona, &e.key)
                .await
                .unwrap();
        put_session_variable(
            &mut out,
            &SessionWrite {
                context_key: "chat-1",
                focus_id: FOCUS,
                name: "locked_intent",
                value: "billing",
                written_by_run: "run-1",
                expected_version: None,
            },
        )
        .await
        .unwrap();

        let s = withdraw(&e).await.unwrap();
        assert_eq!(s.consent, CollectionConsent::Withdrawn);
        assert_eq!(s.stored_version, 0);
        assert!(!s.needs_decision, "a withdrawn user is not asked again");
        // Collected data is kept: a withdrawal deletes nothing.
        let kept = get_session_variable(&mut out, "chat-1", FOCUS, "locked_intent")
            .await
            .unwrap();
        assert_eq!(kept.map(|v| v.value).as_deref(), Some("billing"));

        // A later grant points at the withdrawal.
        grant(&e, cv()).await.unwrap();
        let ev = events(&e).await;
        let kinds: Vec<&str> = ev.iter().map(|x| x.event_type.as_str()).collect();
        assert_eq!(
            kinds,
            [
                "collection_consent_granted",
                "collection_consent_withdrawn",
                "collection_consent_granted"
            ]
        );
        assert_eq!(ev[0].consent_reference_id, None);
        assert_eq!(
            ev[1].consent_reference_id.as_deref(),
            Some(ev[0].id.as_str())
        );
        assert_eq!(
            ev[2].consent_reference_id.as_deref(),
            Some(ev[1].id.as_str())
        );
        assert_eq!(
            ev[1].text_version, ev[0].text_version,
            "the withdrawn grant's text"
        );
        assert!(state(&e).await.is_current_grant());
    }

    #[tokio::test]
    async fn only_a_grant_can_be_withdrawn() {
        let e = env().await;
        assert_eq!(withdraw(&e).await.unwrap_err(), "no_consent_to_withdraw");
        decline(&e, cv()).await.unwrap();
        assert_eq!(withdraw(&e).await.unwrap_err(), "no_consent_to_withdraw");
        assert!(events(&e).await.is_empty());
    }

    // -- who may call --------------------------------------------------------

    #[tokio::test]
    async fn another_users_persona_and_non_uuid_ids_are_refused_and_nothing_changes() {
        let e = env().await;
        let intruder = add_user(&e.pool, "Intruder").await;

        // Another account's Persona: the same error as one that does not exist.
        for r in [
            get_state_for_session(&e.pool, &intruder, &e.persona, FOCUS)
                .await
                .map(|_| ()),
            submit_decision_for_session(
                &e.pool,
                &intruder,
                &e.key,
                &e.persona,
                FOCUS,
                CollectionConsentDecision::Granted,
                cv(),
                &no_hook,
            )
            .await
            .map(|_| ()),
            withdraw_for_session(&e.pool, &intruder, &e.key, &e.persona, FOCUS, &no_hook)
                .await
                .map(|_| ()),
            list_events_for_session(&e.pool, &intruder, &e.key, &e.persona, FOCUS)
                .await
                .map(|_| ()),
        ] {
            assert_eq!(r.unwrap_err(), "persona_not_found");
        }
        assert_eq!(state(&e).await.consent, CollectionConsent::NotAsked);
        assert!(events(&e).await.is_empty());

        // A Persona that is a member but whose id is not a UUID (an old-style id,
        // upper case, or no hyphens) is refused before any store opens.
        let odd = [
            "persona-1",
            &e.persona.to_uppercase(),
            &e.persona.replace('-', ""),
            "",
        ];
        for id in odd {
            if id == e.persona {
                continue;
            }
            assert_eq!(
                get_state_for_session(&e.pool, &e.user, id, FOCUS)
                    .await
                    .unwrap_err(),
                "persona_not_found",
                "{id:?}"
            );
            assert_eq!(
                grant_as(&e, id).await.unwrap_err(),
                "persona_not_found",
                "{id:?}"
            );
        }
        sqlx::query("INSERT INTO personas (id, created_at) VALUES ('persona-1', 't')")
            .execute(&e.pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO user_personas (user_id, persona_id, role) VALUES (?, 'persona-1', 'owner')")
            .bind(&e.user)
            .execute(&e.pool)
            .await
            .ok();
        assert_eq!(
            get_state_for_session(&e.pool, &e.user, "persona-1", FOCUS)
                .await
                .unwrap_err(),
            "persona_not_found",
            "membership does not rescue a non-UUID id"
        );
    }

    async fn grant_as(e: &Env, persona: &str) -> Result<CollectionConsentState, String> {
        submit_decision_for_session(
            &e.pool,
            &e.user,
            &e.key,
            persona,
            FOCUS,
            CollectionConsentDecision::Granted,
            cv(),
            &no_hook,
        )
        .await
    }

    #[tokio::test]
    async fn a_bad_focus_id_and_a_missing_settings_row_are_refused() {
        let e = env().await;
        for bad in ["", "Tech Support", "../x", &"a".repeat(65)] {
            assert_eq!(
                get_state_for_session(&e.pool, &e.user, &e.persona, bad)
                    .await
                    .unwrap_err(),
                "invalid_focus_id",
                "{bad:?}"
            );
        }
        assert_eq!(
            get_state_for_session(&e.pool, &e.user, &e.persona, "no-such-focus")
                .await
                .unwrap_err(),
            "focus_settings_not_found"
        );
        // A grant for a Focus with no settings row appends no audit event.
        let r = submit_decision_for_session(
            &e.pool,
            &e.user,
            &e.key,
            &e.persona,
            "no-such-focus",
            CollectionConsentDecision::Granted,
            cv(),
            &no_hook,
        )
        .await;
        assert_eq!(r.unwrap_err(), "focus_settings_not_found");
        assert!(
            list_events_for_session(&e.pool, &e.user, &e.key, &e.persona, "no-such-focus")
                .await
                .unwrap()
                .is_empty()
        );
    }

    /// The commands themselves: not logged in is refused; logged in they derive
    /// the user and key from the session and act on that user's Persona.
    #[tokio::test]
    async fn the_commands_need_a_session_and_use_the_sessions_user() {
        let e = env().await;
        let app = mock_app_with_registry(e.pool.clone());

        let registry = app.state::<KeyRegistry>();
        let pool = app.state::<sqlx::SqlitePool>();
        let r = get_collection_consent_state(
            e.persona.clone(),
            FOCUS.to_owned(),
            registry.clone(),
            pool.clone(),
        )
        .await;
        assert!(r.is_err(), "not logged in");
        let r = submit_collection_consent_decision(
            e.persona.clone(),
            FOCUS.to_owned(),
            CollectionConsentDecision::Granted,
            cv() as i32,
            registry.clone(),
            pool.clone(),
        )
        .await;
        assert!(r.is_err(), "not logged in");
        assert_eq!(state(&e).await.consent, CollectionConsent::NotAsked);

        // The session user is the Persona's owner (ids come from the session).
        let master = MASTER_KEY;
        // populate_registry takes the user id to unlock as.
        populate_registry(&registry, &e.user, master).await;
        let s = submit_collection_consent_decision(
            e.persona.clone(),
            FOCUS.to_owned(),
            CollectionConsentDecision::Granted,
            cv() as i32,
            registry.clone(),
            pool.clone(),
        )
        .await
        .unwrap();
        assert_eq!(s.consent, CollectionConsent::Granted);
        let listed = list_consent_audit_events(
            e.persona.clone(),
            FOCUS.to_owned(),
            registry.clone(),
            pool.clone(),
        )
        .await
        .unwrap();
        assert_eq!(listed.len(), 1);
        let w = withdraw_collection_consent(
            e.persona.clone(),
            FOCUS.to_owned(),
            registry.clone(),
            pool.clone(),
        )
        .await
        .unwrap();
        assert_eq!(w.consent, CollectionConsent::Withdrawn);
        let g = get_collection_consent_state(e.persona.clone(), FOCUS.to_owned(), registry, pool)
            .await
            .unwrap();
        assert!(!g.needs_decision);
    }

    // -- shape ---------------------------------------------------------------

    /// Each command takes the writer gate first, derives the user from the
    /// session, takes no user_id and no AppHandle, and carries its own body.
    #[test]
    fn every_command_takes_the_gate_first_and_derives_the_user_from_the_session() {
        let src = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("src/commands/collection_consent.rs"),
        )
        .unwrap();
        let production = src.split("\n#[cfg(test)]").next().unwrap();
        let commands: Vec<&str> = production.split("#[tauri::command]").skip(1).collect();
        assert_eq!(commands.len(), 4);
        for body in commands {
            let sig_end = body.find("{\n").unwrap();
            let sig = &body[..sig_end];
            assert!(!sig.contains("user_id"), "no user_id argument: {sig}");
            assert!(!sig.contains("AppHandle"), "{sig}");
            assert!(!sig.contains("key_hex"), "no key argument: {sig}");
            let after = &body[sig_end..];
            let gate = after
                .find("writer_gate::command(&key_registry)")
                .expect("gate");
            let session = after
                .find("session_user_and_key(&key_registry)")
                .expect("session");
            assert!(gate < session, "the gate comes first");
            assert!(after.contains("_for_session("), "a _for_session body");
        }
    }
}
