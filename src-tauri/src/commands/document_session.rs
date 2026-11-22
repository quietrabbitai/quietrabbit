// src-tauri/src/commands/document_session.rs
//
// items.id=903 unit B (decisions.id=942 Q14 to Q16, decisions.id=963): the DOCUMENT
// PRIVACY FLAG command. No screen (items.id=912 shows it).
//
//   set_document_privacy_flag(persona_id, context_key, focus_id, flag, output_id)
//       the user's own label for the document this chat is writing, chosen after the
//       output scan: general, personal, medical, financial or legal. Stored for the
//       chat and Focus with the run's document type (the chat's launch answer, or the
//       question's default), and stamped on `output_id` when given (a saved output of
//       a run of this same chat and Focus; any other id is refused). medical,
//       financial and legal make every later pass of the document local-only
//       (conductor::lifecycle resolve_tier_config, document_session_store::
//       restricted_ceiling), until the one-document raise of unit D applies.
//       Returns the stored state.
//
// WHAT THIS NEVER DOES: raise a ceiling, approve a hosted or metered step, or edit
// focus_settings. A flag only labels and tightens. A source scan pins that this module
// never names the spend gate, a provider, a plan or the raise.
//
// THE ONE-TIME LATCH is obeyed with no exemption: a latched chat refuses the flag
// (one_time_chat), since a one-time chat stores nothing; a latch that cannot be read
// fails it closed (document_unavailable).
//
// The same shape as the launch commands: writer_gate::command first, the user and key
// from the unlocked session, the Persona id a canonical UUID of that user, and the work
// in a *_for_session body tested directly.
//
// ERROR CODES (ERROR_CODES, each with a placeholder key document.error.<code> in the
// frontend's en.json; wording is Chat-BRAND's): persona_not_found, invalid_focus_id,
// invalid_context_key, no_document_type, invalid_output_id, one_time_chat,
// document_unavailable.

use serde::Serialize;
use specta::Type;
use tauri::State;

use crate::auth::registry::KeyRegistry;
use crate::commands::persona::session_user_and_key;
use crate::conductor::launch_config::{self, SLOT_DOCUMENT_TYPE};
use crate::conductor::model_block::document_type::registry;
use crate::persistence::document_session_store::{
    self, DocumentPrivacyFlag, DocumentSession, DocumentSessionError,
};
use crate::persistence::session_store::LatchError;
use crate::persistence::{launch_answer_store, output_store, persona_store, session_store};

/// Every error string this module returns.
pub const ERROR_CODES: [&str; 7] = [
    "persona_not_found",
    "invalid_focus_id",
    "invalid_context_key",
    "no_document_type",
    "invalid_output_id",
    "one_time_chat",
    "document_unavailable",
];

/// The chat's document state, as stored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Type)]
pub struct DocumentSessionView {
    pub focus_id: String,
    pub document_type: String,
    pub document_privacy_flag: Option<DocumentPrivacyFlag>,
    /// The flag keeps later passes of this document on the device.
    pub local_only: bool,
}

fn view(s: &DocumentSession) -> DocumentSessionView {
    DocumentSessionView {
        focus_id: s.focus_id.clone(),
        document_type: s.document_type.clone(),
        document_privacy_flag: s.document_privacy_flag,
        local_only: document_session_store::restricted_ceiling(Some(s), &s.document_type).is_some(),
    }
}

fn unavailable<E: std::fmt::Display>(what: &str, e: E) -> String {
    log::warn!("document_session: {what}: {e}");
    "document_unavailable".to_owned()
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

fn store_error(e: DocumentSessionError) -> String {
    match e {
        DocumentSessionError::Latch(LatchError::Suppressed) => "one_time_chat".to_owned(),
        other => unavailable("document session store", other),
    }
}

#[tauri::command]
#[specta::specta]
pub async fn set_document_privacy_flag(
    persona_id: String,
    context_key: String,
    focus_id: String,
    flag: DocumentPrivacyFlag,
    output_id: Option<String>,
    key_registry: State<'_, KeyRegistry>,
    pool: State<'_, sqlx::SqlitePool>,
) -> Result<DocumentSessionView, String> {
    let _lease = crate::auth::writer_gate::command(&key_registry).await?;
    let (user_id, key_hex) = session_user_and_key(&key_registry).await?;
    set_document_privacy_flag_for_session(
        &pool,
        &user_id,
        &key_hex,
        &persona_id,
        &context_key,
        &focus_id,
        flag,
        output_id.as_deref(),
    )
    .await
}

// 8 arguments: the pool, the session's user and key, and the five the command takes.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn set_document_privacy_flag_for_session(
    pool: &sqlx::SqlitePool,
    user_id: &str,
    key_hex: &str,
    persona_id: &str,
    context_key: &str,
    focus_id: &str,
    flag: DocumentPrivacyFlag,
    output_id: Option<&str>,
) -> Result<DocumentSessionView, String> {
    require_persona(pool, user_id, persona_id).await?;
    require_focus_id(focus_id)?;
    session_store::validate_context_key(context_key)
        .map_err(|_| "invalid_context_key".to_owned())?;
    // The document type: only a Focus that asks (or fixes) one has documents.
    let reg = registry().map_err(|e| unavailable("document types", e))?;
    let config = launch_config::read_validated(pool, focus_id, reg)
        .await
        .map_err(|e| unavailable("launch config", e))?;
    let question = config
        .as_ref()
        .and_then(|c| c.question_for_slot(SLOT_DOCUMENT_TYPE))
        .ok_or_else(|| "no_document_type".to_owned())?;
    let mut conn = output_store::open_outputs_db_retrying(user_id, persona_id, key_hex)
        .await
        .map_err(|e| unavailable("open outputs.db", e))?;
    let stored = launch_answer_store::read_answers(&mut conn, context_key, focus_id)
        .await
        .map_err(|e| unavailable("read the stored answers", e))?;
    // The same rule as the run start: an asked question's valid answer, else its default.
    let document_type = match stored.get(question.slot.as_str()) {
        Some(v) if question.asked && question.option(v).is_some() => v.clone(),
        _ => question.default_option.clone(),
    };
    // The output first: an id that is not this chat's and Focus's refuses the whole
    // command before anything is written.
    if let Some(id) = output_id {
        if id.is_empty() || id.len() > 128 {
            return Err("invalid_output_id".to_owned());
        }
        let stamped =
            document_session_store::stamp_output_flag(&mut conn, id, context_key, focus_id, flag)
                .await
                .map_err(store_error)?;
        if !stamped {
            return Err("invalid_output_id".to_owned());
        }
    }
    let session = document_session_store::set_privacy_flag(
        &mut conn,
        context_key,
        focus_id,
        &document_type,
        flag,
    )
    .await
    .map_err(store_error)?;
    Ok(view(&session))
}

#[cfg(test)]
#[path = "document_session_tests.rs"]
mod tests;
