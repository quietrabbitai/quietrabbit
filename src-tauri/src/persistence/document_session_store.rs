// src-tauri/src/persistence/document_session_store.rs
//
// items.id=903 unit B (decisions.id=942 Q14 to Q16, decisions.id=963): the typed store of
// document_sessions (schema/outputs_020.sql) in the persona's encrypted outputs.db, and
// the document privacy flag stamped on a saved output.
//
// One row per (context_key, focus_id): the document type the row was written for, the
// user's privacy flag for the document, and (unit D) a one-document ceiling raise with the
// id of its consent audit row. NO TEXT of the document is stored here.
//
// WRITES ARE LATCH-GATED, as every chat-scoped writer is (session_store::ensure_writable
// inside one BEGIN IMMEDIATE): a one-time chat refuses the flag (LatchError::Suppressed),
// and a latch that cannot be read refuses it too (fail closed). Each write moves version up
// by one with a compare-and-set on the version it read.
//
// A CHANGED DOCUMENT TYPE. Setting the flag for another document type than the row holds
// rewrites the type and VOIDS any raise (raised_ceiling and consent_audit_id go NULL): a
// raise was given for one document. The flag itself is kept or replaced by the new value.
//
// READS. get() for the commands and the run start. The run's ceiling rule is
// effective_ceiling(): a flag of medical, financial or legal limits the run to
// local_only (restricted_ceiling), and a one-document raise of the same document type
// lifts it to the raised tier; the caller then clamps to the Focus's own ceiling.
//
// THE RAISE (unit D) is written only by set_raise, whose one caller is
// commands/document_raise.rs, after the consent audit row (a source scan pins it).
//
// DELETES. Only the archive hook (clear_chat_in_tx, persistence::chat_clear) and the
// 30-day store sweep (sweep_older_than_in_tx, persistence::store_sweep). Neither is
// latch-gated: they only remove.
//
// Logging: nothing here logs. A row carries no user text, but the module still logs
// nothing so a later column cannot leak through a log line.

use sqlx::{Row, SqliteConnection};
use thiserror::Error;

use crate::conductor::tokens::ExternalAccess;
use crate::persistence::session_store::{
    begin_write, ensure_writable, finish_write, validate_context_key, validate_text, LatchError,
};

const FOCUS_ID_MAX_CHARS: usize = 128;
const DOCUMENT_TYPE_MAX_CHARS: usize = 64;

/// The user's label for a document. Closed, held here and not as a SQL CHECK (see
/// outputs_020.sql); a round-trip test keeps ALL and the text together.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize, specta::Type,
)]
#[serde(rename_all = "snake_case")]
pub enum DocumentPrivacyFlag {
    General,
    Personal,
    Medical,
    Financial,
    Legal,
}

impl DocumentPrivacyFlag {
    pub const ALL: [DocumentPrivacyFlag; 5] = [
        DocumentPrivacyFlag::General,
        DocumentPrivacyFlag::Personal,
        DocumentPrivacyFlag::Medical,
        DocumentPrivacyFlag::Financial,
        DocumentPrivacyFlag::Legal,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            DocumentPrivacyFlag::General => "general",
            DocumentPrivacyFlag::Personal => "personal",
            DocumentPrivacyFlag::Medical => "medical",
            DocumentPrivacyFlag::Financial => "financial",
            DocumentPrivacyFlag::Legal => "legal",
        }
    }

    pub fn parse(s: &str) -> Option<DocumentPrivacyFlag> {
        DocumentPrivacyFlag::ALL
            .iter()
            .copied()
            .find(|f| f.as_str() == s)
    }

    /// medical, financial and legal keep every later pass of the document on the
    /// device (decisions.id=942 Q14: the Path C default).
    pub fn restricts_to_local(self) -> bool {
        matches!(
            self,
            DocumentPrivacyFlag::Medical
                | DocumentPrivacyFlag::Financial
                | DocumentPrivacyFlag::Legal
        )
    }
}

#[derive(Debug, Error)]
pub enum DocumentSessionError {
    #[error("Database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("{0}")]
    Validation(String),
    #[error("document session version conflict")]
    VersionConflict,
    #[error("{0}")]
    Latch(#[from] LatchError),
    /// items.id=903 unit D: no document session for the chat, or one of another document
    /// type: there is no document to raise the ceiling for.
    #[error("no document of this type in this chat")]
    NoDocument,
    /// A stored value outside its closed set: a corrupt or foreign row. Never skipped.
    #[error("document_sessions holds an unknown {0}")]
    Corrupt(&'static str),
}

/// One row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocumentSession {
    pub context_key: String,
    pub focus_id: String,
    pub document_type: String,
    pub document_privacy_flag: Option<DocumentPrivacyFlag>,
    pub raised_ceiling: Option<ExternalAccess>,
    pub consent_audit_id: Option<String>,
    pub version: i64,
    pub created_at: String,
    pub updated_at: String,
}

fn validate_scope(context_key: &str, focus_id: &str) -> Result<(), DocumentSessionError> {
    validate_context_key(context_key).map_err(DocumentSessionError::Validation)?;
    validate_text("focus_id", focus_id, FOCUS_ID_MAX_CHARS)
        .map_err(DocumentSessionError::Validation)
}

const COLUMNS: &str = "context_key, focus_id, document_type, document_privacy_flag, \
                       raised_ceiling, consent_audit_id, version, created_at, updated_at";

fn row_to_session(r: &sqlx::sqlite::SqliteRow) -> Result<DocumentSession, DocumentSessionError> {
    let flag: Option<String> = r.try_get("document_privacy_flag")?;
    let flag = match flag {
        None => None,
        Some(s) => Some(
            DocumentPrivacyFlag::parse(&s).ok_or(DocumentSessionError::Corrupt("privacy flag"))?,
        ),
    };
    let raised: Option<String> = r.try_get("raised_ceiling")?;
    let raised = match raised {
        None => None,
        Some(s) => Some(
            s.parse::<ExternalAccess>()
                .map_err(|_| DocumentSessionError::Corrupt("raised ceiling"))?,
        ),
    };
    Ok(DocumentSession {
        context_key: r.try_get("context_key")?,
        focus_id: r.try_get("focus_id")?,
        document_type: r.try_get("document_type")?,
        document_privacy_flag: flag,
        raised_ceiling: raised,
        consent_audit_id: r.try_get("consent_audit_id")?,
        version: r.try_get("version")?,
        created_at: r.try_get("created_at")?,
        updated_at: r.try_get("updated_at")?,
    })
}

async fn get_inner(
    conn: &mut SqliteConnection,
    context_key: &str,
    focus_id: &str,
) -> Result<Option<DocumentSession>, DocumentSessionError> {
    let row = sqlx::query(&format!(
        "SELECT {COLUMNS} FROM document_sessions WHERE context_key = ? AND focus_id = ?"
    ))
    .bind(context_key)
    .bind(focus_id)
    .fetch_optional(&mut *conn)
    .await?;
    row.as_ref().map(row_to_session).transpose()
}

/// The chat's document session for the Focus, or None. A read error is an Err: a
/// caller must not treat it as absent (the run start fails closed on it).
pub async fn get(
    conn: &mut SqliteConnection,
    context_key: &str,
    focus_id: &str,
) -> Result<Option<DocumentSession>, DocumentSessionError> {
    validate_scope(context_key, focus_id)?;
    get_inner(conn, context_key, focus_id).await
}

/// Set (or replace) the document privacy flag for the chat's document of `document_type`.
/// Latch-gated, in one BEGIN IMMEDIATE, compare-and-set on the version it read. A row of
/// another document type is rewritten to this one and its raise is voided. Returns the
/// row as written.
pub async fn set_privacy_flag(
    conn: &mut SqliteConnection,
    context_key: &str,
    focus_id: &str,
    document_type: &str,
    flag: DocumentPrivacyFlag,
) -> Result<DocumentSession, DocumentSessionError> {
    validate_scope(context_key, focus_id)?;
    validate_text("document_type", document_type, DOCUMENT_TYPE_MAX_CHARS)
        .map_err(DocumentSessionError::Validation)?;
    begin_write(conn).await?;
    let result = async {
        ensure_writable(conn, context_key).await?;
        let now = crate::providers::utils::now();
        match get_inner(conn, context_key, focus_id).await? {
            None => {
                sqlx::query(&format!(
                    "INSERT INTO document_sessions ({COLUMNS})
                     VALUES (?, ?, ?, ?, NULL, NULL, 1, ?, ?)"
                ))
                .bind(context_key)
                .bind(focus_id)
                .bind(document_type)
                .bind(flag.as_str())
                .bind(&now)
                .bind(&now)
                .execute(&mut *conn)
                .await?;
            }
            Some(current) => {
                let same_type = current.document_type == document_type;
                let done = sqlx::query(
                    "UPDATE document_sessions
                     SET document_type = ?, document_privacy_flag = ?,
                         raised_ceiling = CASE WHEN ? THEN raised_ceiling ELSE NULL END,
                         consent_audit_id = CASE WHEN ? THEN consent_audit_id ELSE NULL END,
                         version = version + 1, updated_at = ?
                     WHERE context_key = ? AND focus_id = ? AND version = ?",
                )
                .bind(document_type)
                .bind(flag.as_str())
                .bind(same_type)
                .bind(same_type)
                .bind(&now)
                .bind(context_key)
                .bind(focus_id)
                .bind(current.version)
                .execute(&mut *conn)
                .await?;
                if done.rows_affected() != 1 {
                    return Err(DocumentSessionError::VersionConflict);
                }
            }
        }
        get_inner(conn, context_key, focus_id)
            .await?
            .ok_or(DocumentSessionError::VersionConflict)
    }
    .await;
    finish_write(conn, result).await
}

/// Stamp the flag on a saved output of this chat and Focus (outputs.document_privacy_flag).
/// Only an output produced by a run of the same chat and Focus is stamped; any other id
/// changes nothing and returns false. Latch-gated like every chat-scoped write.
pub async fn stamp_output_flag(
    conn: &mut SqliteConnection,
    output_id: &str,
    context_key: &str,
    focus_id: &str,
    flag: DocumentPrivacyFlag,
) -> Result<bool, DocumentSessionError> {
    validate_scope(context_key, focus_id)?;
    validate_text("output_id", output_id, 128).map_err(DocumentSessionError::Validation)?;
    begin_write(conn).await?;
    let result = async {
        ensure_writable(conn, context_key).await?;
        let done = sqlx::query(
            "UPDATE outputs SET document_privacy_flag = ?
             WHERE id = ? AND focus_run_id IN
                 (SELECT id FROM focus_runs WHERE context_key = ? AND focus_id = ?)",
        )
        .bind(flag.as_str())
        .bind(output_id)
        .bind(context_key)
        .bind(focus_id)
        .execute(&mut *conn)
        .await?;
        Ok::<bool, DocumentSessionError>(done.rows_affected() == 1)
    }
    .await;
    finish_write(conn, result).await
}

/// The run's ceiling rule for its document (unit B): Some(local_only) when the chat's
/// document carries a flag that restricts to local, None otherwise. `run_document_type`
/// is the run's selected type; the flag applies whatever it is (a flag only tightens).
pub fn restricted_ceiling(
    session: Option<&DocumentSession>,
    _run_document_type: &str,
) -> Option<ExternalAccess> {
    let s = session?;
    s.document_privacy_flag
        .filter(|f| f.restricts_to_local())
        .map(|_| ExternalAccess::LocalOnly)
}

/// items.id=903 unit D (decisions.id=942 Q14 Path B): record a ONE-DOCUMENT ceiling
/// raise, after its consent audit row was appended (`consent_audit_id`). Only
/// commands/document_raise.rs calls this (a source scan pins it): no model path, store
/// operation or lifecycle code can raise a ceiling. Latch-gated, compare-and-set, and
/// only for a row of the same document type (else NoDocument): a raise is for one
/// document, and a changed type voids it (set_privacy_flag).
pub(crate) async fn set_raise(
    conn: &mut SqliteConnection,
    context_key: &str,
    focus_id: &str,
    document_type: &str,
    tier: ExternalAccess,
    consent_audit_id: &str,
) -> Result<DocumentSession, DocumentSessionError> {
    validate_scope(context_key, focus_id)?;
    validate_text("consent_audit_id", consent_audit_id, 128)
        .map_err(DocumentSessionError::Validation)?;
    begin_write(conn).await?;
    let result = async {
        ensure_writable(conn, context_key).await?;
        let current = get_inner(conn, context_key, focus_id)
            .await?
            .filter(|s| s.document_type == document_type)
            .ok_or(DocumentSessionError::NoDocument)?;
        let done = sqlx::query(
            "UPDATE document_sessions
             SET raised_ceiling = ?, consent_audit_id = ?, version = version + 1, updated_at = ?
             WHERE context_key = ? AND focus_id = ? AND version = ?",
        )
        .bind(tier.as_str())
        .bind(consent_audit_id)
        .bind(crate::providers::utils::now())
        .bind(context_key)
        .bind(focus_id)
        .bind(current.version)
        .execute(&mut *conn)
        .await?;
        if done.rows_affected() != 1 {
            return Err(DocumentSessionError::VersionConflict);
        }
        get_inner(conn, context_key, focus_id)
            .await?
            .ok_or(DocumentSessionError::VersionConflict)
    }
    .await;
    finish_write(conn, result).await
}

/// items.id=903 unit D: the run's permitted tier for its document, from the user's
/// focus_settings ceiling (`user_max`):
///   1. a flag of medical, financial or legal makes it local_only (restricted_ceiling);
///   2. a ONE-DOCUMENT raise of this same document type lifts it to the raised tier
///      (overriding the flag for this document only).
///
/// The caller then takes the minimum with the Focus's own ceiling, so nothing here can
/// exceed it, and nothing here edits focus_settings.
pub fn effective_ceiling(
    session: Option<&DocumentSession>,
    run_document_type: &str,
    user_max: ExternalAccess,
) -> ExternalAccess {
    let base = match restricted_ceiling(session, run_document_type) {
        Some(cap) => user_max.min(cap),
        None => user_max,
    };
    match session.and_then(|s| {
        s.raised_ceiling
            .filter(|_| s.document_type == run_document_type)
    }) {
        Some(raised) => base.max(raised),
        None => base,
    }
}

/// items.id=903: remove the chat's document sessions, inside a write transaction the
/// caller holds (persistence::chat_clear, the archive hook). Not latch-gated.
pub(crate) async fn clear_chat_in_tx(
    conn: &mut SqliteConnection,
    context_key: &str,
) -> Result<u64, DocumentSessionError> {
    let done = sqlx::query("DELETE FROM document_sessions WHERE context_key = ?")
        .bind(context_key)
        .execute(&mut *conn)
        .await?;
    Ok(done.rows_affected())
}

/// items.id=903: remove the rows last written before `cutoff` (RFC 3339 text, as every
/// writer here stamps through providers::utils::now()), inside a write transaction the
/// caller holds (persistence::store_sweep). Not latch-gated.
pub(crate) async fn sweep_older_than_in_tx(
    conn: &mut SqliteConnection,
    cutoff: &str,
) -> Result<u64, DocumentSessionError> {
    let done = sqlx::query("DELETE FROM document_sessions WHERE updated_at < ?")
        .bind(cutoff)
        .execute(&mut *conn)
        .await?;
    Ok(done.rows_affected())
}

#[cfg(test)]
#[path = "document_session_store_tests.rs"]
mod tests;
