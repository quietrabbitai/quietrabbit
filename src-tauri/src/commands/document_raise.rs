// src-tauri/src/commands/document_raise.rs
//
// items.id=903 unit D (decisions.id=942 Q14 Path B, decisions.id=944, decisions.id=963
// rulings 5 and 7): the ONE-DOCUMENT CEILING RAISE commands. No screen (items.id=912).
// The rules and the challenge tokens are conductor::document_raise.
//
//   request_document_ceiling_raise(persona_id, context_key, focus_id, target)
//       reads everything fresh (the persona's type, the Focus's focus_profile and the
//       user's ceiling, the loaded Focus's own ceiling, the chat's latch and document
//       session, the run's document type) and either refuses with its code or returns a
//       challenge: a single-use token in memory, the fixed disclosure (document type,
//       target tier, this document only), the minimum wait and the expiry, and the time
//       of an earlier raise of this document type in this Persona and Focus when there
//       was one (the repeat-consent wording the screen shows).
//   confirm_document_ceiling_raise(token, acknowledged)
//       refuses an unknown, foreign, used or expired token, a confirm before the wait
//       (the token stays valid), and acknowledged false. Then it checks the rules AGAIN
//       on fresh reads (the state may have changed since the request), appends the
//       consent audit row FIRST (personal.db, document_ceiling_raised, the document type,
//       a fixed summary, consent_reference_id = the earlier raise's audit id, the text
//       version) and writes the raise SECOND (document_session_store::set_raise, with
//       the audit id). A failure after the audit row leaves the ceiling unraised, and the
//       audit row records the consent that was given; a retry needs a fresh token and
//       appends a new event.
//
// WHAT THIS NEVER DOES: edit focus_settings, approve a hosted or metered step (the run
// plan still needs its own approval, decisions.id=942 Q3), or raise above the Focus's
// own ceiling. These two commands are the ONLY callers of set_raise and the only
// callers of the audit store outside the collection-consent commands (source scans).
//
// The same shape as the other session commands: writer_gate::command first, the user
// and key from the unlocked session, the Persona id a canonical UUID of that user, the
// work in *_for_session bodies tested directly.
//
// ERROR CODES (ERROR_CODES, each with a placeholder document.raise_error.<code> key in
// en.json; wording is Chat-BRAND's).

use std::time::Instant;

use serde::Serialize;
use specta::Type;
use tauri::State;

use crate::auth::registry::KeyRegistry;
use crate::commands::persona::session_user_and_key;
use crate::conductor::document_raise::{
    check, ChallengeScope, RaiseChallenges, RaiseCheck, CHALLENGE_TTL, MIN_WAIT,
    RAISE_AUDIT_SUMMARY, RAISE_TEXT_VERSION,
};
use crate::conductor::launch_config::{self, SLOT_DOCUMENT_TYPE};
use crate::conductor::model_block::document_type::registry;
use crate::conductor::tokens::ExternalAccess;
use crate::persistence::consent_audit_store::{self, ConsentEventType, NewConsentEvent};
use crate::persistence::document_session_store::{self, DocumentSession};
use crate::persistence::personal_store::open_personal_db;
use crate::persistence::{
    focus_settings_store, launch_answer_store, output_store, persona_store, session_store,
};

/// Every error string this module returns.
pub const ERROR_CODES: [&str; 14] = [
    "persona_not_found",
    "invalid_focus_id",
    "invalid_context_key",
    "no_document_type",
    "protected_persona",
    "one_time_chat",
    "no_document",
    "above_focus_ceiling",
    "not_a_raise",
    "invalid_token",
    "too_early",
    "expired",
    "not_acknowledged",
    "raise_unavailable",
];

/// A raise the user is asked to confirm. The screen words the disclosure from these
/// fields (document.raise.* keys); the token is opaque.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Type)]
pub struct RaiseChallengeView {
    pub token: String,
    pub document_type: String,
    pub target: ExternalAccess,
    /// The confirm is refused until this many milliseconds after the request.
    pub wait_ms: u32,
    pub expires_in_ms: u32,
    /// An earlier raise of this document type in this Persona and Focus, as stored.
    pub previous_raise_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Type)]
pub struct DocumentRaiseView {
    pub document_type: String,
    pub raised_ceiling: ExternalAccess,
}

fn unavailable<E: std::fmt::Display>(what: &str, e: E) -> String {
    log::warn!("document_raise: {what}: {e}");
    "raise_unavailable".to_owned()
}

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

fn require_focus_id(focus_id: &str) -> Result<(), String> {
    let ok = !focus_id.is_empty()
        && focus_id.len() <= 64
        && focus_id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_');
    ok.then_some(())
        .ok_or_else(|| "invalid_focus_id".to_owned())
}

/// Who and what a raise is about, as the session and the command gave them.
pub(crate) struct Ids<'a> {
    pub(crate) user_id: &'a str,
    pub(crate) key_hex: &'a str,
    pub(crate) persona_id: &'a str,
    pub(crate) context_key: &'a str,
    pub(crate) focus_id: &'a str,
}

/// Everything the rules read, fresh. Returns the run's document type and the session.
async fn checked(
    pool: &sqlx::SqlitePool,
    ids: &Ids<'_>,
    target: ExternalAccess,
) -> Result<(String, DocumentSession), String> {
    require_persona(pool, ids.user_id, ids.persona_id).await?;
    require_focus_id(ids.focus_id)?;
    session_store::validate_context_key(ids.context_key)
        .map_err(|_| "invalid_context_key".to_owned())?;
    let reg = registry().map_err(|e| unavailable("document types", e))?;
    let config = launch_config::read_validated(pool, ids.focus_id, reg)
        .await
        .map_err(|e| unavailable("launch config", e))?;
    let question = config
        .as_ref()
        .and_then(|c| c.question_for_slot(SLOT_DOCUMENT_TYPE))
        .ok_or_else(|| "no_document_type".to_owned())?;
    // The loaded Focus: its row, its bundled file and the tier manifest, strictest.
    let focus = crate::conductor::lifecycle::load_focus_definition(pool, ids.focus_id)
        .await
        .map_err(|e| unavailable("load the Focus", e))?;
    let settings = focus_settings_store::get_focus_settings(pool, ids.persona_id, ids.focus_id)
        .await
        .map_err(|e| unavailable("focus settings", e))?
        .ok_or_else(|| unavailable("focus settings", "no row for this Focus"))?;
    let persona =
        persona_store::get_persona_for_user(pool, ids.user_id, ids.key_hex, ids.persona_id)
            .await
            .map_err(|e| unavailable("persona", e))?
            .ok_or_else(|| "persona_not_found".to_owned())?;

    let mut conn = output_store::open_outputs_db_retrying(ids.user_id, ids.persona_id, ids.key_hex)
        .await
        .map_err(|e| unavailable("open outputs.db", e))?;
    let latched = matches!(
        session_store::write_mode(&mut conn, ids.context_key)
            .await
            .map_err(|e| unavailable("read the one-time latch", e))?,
        session_store::WriteMode::Suppressed
    );
    let stored = launch_answer_store::read_answers(&mut conn, ids.context_key, ids.focus_id)
        .await
        .map_err(|e| unavailable("read the stored answers", e))?;
    let document_type = match stored.get(question.slot.as_str()) {
        Some(v) if question.asked && question.option(v).is_some() => v.clone(),
        _ => question.default_option.clone(),
    };
    let session = document_session_store::get(&mut conn, ids.context_key, ids.focus_id)
        .await
        .map_err(|e| unavailable("document session", e))?;
    check(&RaiseCheck {
        persona_protected: persona.persona_type == "protected",
        focus_profile_protected: settings.focus_profile == "protected",
        latched,
        session: session.as_ref(),
        run_document_type: &document_type,
        focus_ceiling: focus.max_routing_tier,
        user_max: settings.max_permitted_tier,
        target,
    })
    .map_err(|r| r.code().to_owned())?;
    let session = session.ok_or_else(|| "no_document".to_owned())?;
    Ok((document_type, session))
}

/// The latest earlier raise of this document type in this Persona and Focus.
pub(crate) async fn previous_raise(
    ids: &Ids<'_>,
    document_type: &str,
) -> Result<Option<consent_audit_store::ConsentAuditRecord>, String> {
    let mut personal = open_personal_db(ids.user_id, ids.persona_id, ids.key_hex)
        .await
        .map_err(|e| unavailable("open personal.db", e))?;
    let rows = consent_audit_store::list(&mut personal, ids.focus_id)
        .await
        .map_err(|e| unavailable("read the consent audit", e))?;
    Ok(rows.into_iter().rfind(|r| {
        r.event_type == ConsentEventType::DocumentCeilingRaised
            && r.document_type.as_deref() == Some(document_type)
    }))
}

#[tauri::command]
#[specta::specta]
pub async fn request_document_ceiling_raise(
    persona_id: String,
    context_key: String,
    focus_id: String,
    target: ExternalAccess,
    key_registry: State<'_, KeyRegistry>,
    pool: State<'_, sqlx::SqlitePool>,
    challenges: State<'_, RaiseChallenges>,
) -> Result<RaiseChallengeView, String> {
    let _lease = crate::auth::writer_gate::command(&key_registry).await?;
    let (user_id, key_hex) = session_user_and_key(&key_registry).await?;
    request_for_session(
        &pool,
        &challenges,
        &Ids {
            user_id: &user_id,
            key_hex: &key_hex,
            persona_id: &persona_id,
            context_key: &context_key,
            focus_id: &focus_id,
        },
        target,
        Instant::now(),
    )
    .await
}

pub(crate) async fn request_for_session(
    pool: &sqlx::SqlitePool,
    challenges: &RaiseChallenges,
    ids: &Ids<'_>,
    target: ExternalAccess,
    now: Instant,
) -> Result<RaiseChallengeView, String> {
    let (document_type, _) = checked(pool, ids, target).await?;
    let previous = previous_raise(ids, &document_type).await?;
    let token = challenges
        .issue(
            ChallengeScope {
                user_id: ids.user_id.to_owned(),
                persona_id: ids.persona_id.to_owned(),
                context_key: ids.context_key.to_owned(),
                focus_id: ids.focus_id.to_owned(),
                document_type: document_type.clone(),
                target,
            },
            now,
        )
        .ok_or_else(|| unavailable("issue a token", "no randomness"))?;
    Ok(RaiseChallengeView {
        token,
        document_type,
        target,
        wait_ms: MIN_WAIT.as_millis() as u32,
        expires_in_ms: CHALLENGE_TTL.as_millis() as u32,
        previous_raise_at: previous.map(|r| r.created_at),
    })
}

#[tauri::command]
#[specta::specta]
pub async fn confirm_document_ceiling_raise(
    token: String,
    acknowledged: bool,
    key_registry: State<'_, KeyRegistry>,
    pool: State<'_, sqlx::SqlitePool>,
    challenges: State<'_, RaiseChallenges>,
) -> Result<DocumentRaiseView, String> {
    let _lease = crate::auth::writer_gate::command(&key_registry).await?;
    let (user_id, key_hex) = session_user_and_key(&key_registry).await?;
    confirm_for_session(
        &pool,
        &challenges,
        &user_id,
        &key_hex,
        &token,
        acknowledged,
        Instant::now(),
    )
    .await
}

/// The two writes of a confirmed raise, in order: the audit row, then the raise.
/// Separate so a test can fault the second.
pub(crate) trait RaiseWriter {
    async fn audit(
        &mut self,
        ids: &Ids<'_>,
        document_type: &str,
        previous: Option<&str>,
    ) -> Result<String, String>;
    async fn raise(
        &mut self,
        ids: &Ids<'_>,
        document_type: &str,
        target: ExternalAccess,
        audit_id: &str,
    ) -> Result<DocumentSession, String>;
}

/// The real writes.
pub(crate) struct StoreRaiseWriter;

impl RaiseWriter for StoreRaiseWriter {
    async fn audit(
        &mut self,
        ids: &Ids<'_>,
        document_type: &str,
        previous: Option<&str>,
    ) -> Result<String, String> {
        let mut personal = open_personal_db(ids.user_id, ids.persona_id, ids.key_hex)
            .await
            .map_err(|e| unavailable("open personal.db", e))?;
        consent_audit_store::append(
            &mut personal,
            &NewConsentEvent {
                event_type: ConsentEventType::DocumentCeilingRaised,
                focus_slug: ids.focus_id,
                run_id: None,
                text_version: Some(RAISE_TEXT_VERSION),
                consent_reference_id: previous,
                content_summary: RAISE_AUDIT_SUMMARY,
                document_type: Some(document_type),
            },
        )
        .await
        .map_err(|e| unavailable("append the consent audit", e))
    }

    async fn raise(
        &mut self,
        ids: &Ids<'_>,
        document_type: &str,
        target: ExternalAccess,
        audit_id: &str,
    ) -> Result<DocumentSession, String> {
        let mut conn =
            output_store::open_outputs_db_retrying(ids.user_id, ids.persona_id, ids.key_hex)
                .await
                .map_err(|e| unavailable("open outputs.db", e))?;
        document_session_store::set_raise(
            &mut conn,
            ids.context_key,
            ids.focus_id,
            document_type,
            target,
            audit_id,
        )
        .await
        .map_err(|e| match e {
            document_session_store::DocumentSessionError::NoDocument => "no_document".to_owned(),
            document_session_store::DocumentSessionError::Latch(
                session_store::LatchError::Suppressed,
            ) => "one_time_chat".to_owned(),
            other => unavailable("record the raise", other),
        })
    }
}

pub(crate) async fn confirm_for_session(
    pool: &sqlx::SqlitePool,
    challenges: &RaiseChallenges,
    user_id: &str,
    key_hex: &str,
    token: &str,
    acknowledged: bool,
    now: Instant,
) -> Result<DocumentRaiseView, String> {
    confirm_with(
        pool,
        challenges,
        user_id,
        key_hex,
        token,
        acknowledged,
        now,
        &mut StoreRaiseWriter,
    )
    .await
}

// 8 arguments: the pool, the token store, the session's user and key, the two the
// command takes, the clock and the writer seam.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn confirm_with(
    pool: &sqlx::SqlitePool,
    challenges: &RaiseChallenges,
    user_id: &str,
    key_hex: &str,
    token: &str,
    acknowledged: bool,
    now: Instant,
    writer: &mut impl RaiseWriter,
) -> Result<DocumentRaiseView, String> {
    if !acknowledged {
        // Not consumed: the user can still confirm deliberately.
        return Err("not_acknowledged".to_owned());
    }
    let scope = challenges
        .take(token, user_id, now)
        .map_err(|r| r.code().to_owned())?;
    let ids = Ids {
        user_id,
        key_hex,
        persona_id: &scope.persona_id,
        context_key: &scope.context_key,
        focus_id: &scope.focus_id,
    };
    // The rules again, on fresh reads, and the same document type as the challenge.
    let (document_type, _) = checked(pool, &ids, scope.target).await?;
    if document_type != scope.document_type {
        return Err("no_document".to_owned());
    }
    let previous = previous_raise(&ids, &document_type).await?;
    // FIRST the audit row, SECOND the raise.
    let audit_id = writer
        .audit(
            &ids,
            &document_type,
            previous.as_ref().map(|r| r.id.as_str()),
        )
        .await?;
    let session = writer
        .raise(&ids, &document_type, scope.target, &audit_id)
        .await?;
    Ok(DocumentRaiseView {
        document_type: session.document_type,
        raised_ceiling: session.raised_ceiling.unwrap_or(scope.target),
    })
}

#[cfg(test)]
#[path = "document_raise_tests.rs"]
mod tests;
