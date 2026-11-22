// src-tauri/src/persistence/pass_artifact_store.rs
//
// items.id=895 (decisions.id=942 architecture point 5, decisions.id=938 ruling
// 2): the typed store for pass_artifacts (schema/outputs_015.sql) in the
// persona's outputs.db. It holds the text a later chat pass needs from an
// earlier one and that does not fit a session variable (2048 bytes): a content
// brief, voice guidance, a validation report and a list of delta items.
//
// Typed API only. The writers a Focus uses are the artifact_put and
// artifact_clear store operations (conductor::store_op, store_writer), and the
// reader is the bound-name seeding of conductor::seeding. There is no other
// path to this table.
//
// CONNECTIONS: every function takes a caller-supplied `&mut SqliteConnection`
// to the persona's outputs.db, opened by output_store::open_outputs_db. The
// persona is the file, so nothing here takes a persona id and a function can
// never read another persona's rows. A connection must not already be inside a
// transaction: each write opens its own.
//
// SAME RULES AS session_store (decisions.id=939), by reusing its helpers:
//   * every write runs in ONE `BEGIN IMMEDIATE` transaction that takes SQLite's
//     write lock before it reads anything, so the version compare-and-set and
//     the one-time latch check are atomic with the write;
//   * every write is refused once the chat is a one-time session (the latch is
//     a row of session_variables, session_store::ensure_writable), and fails
//     closed when the latch cannot be read;
//   * one row per (context_key, focus_id, kind), so a Focus never reads another
//     Focus's artifact and a chat never reads another chat's.
//
// PRIVACY (decisions.id=937, items.id=879). An artifact is text that can echo
// what the user wrote. Here:
//   * it stays in the persona's encrypted outputs.db and nowhere else;
//   * Debug on PassArtifact and ArtifactWrite shows the kind, version, byte
//     count and a short hash, never the text, and no error in this module
//     carries content;
//   * the module logs nothing (a source scan pins it). A caller that logs an
//     artifact write logs fingerprint(), the count and the hash.
//
// QUERY STYLE: runtime sqlx::query() only, never the query!() macros.

use std::fmt;

use sha2::{Digest, Sha256};
use sqlx::{Row, SqliteConnection};
use thiserror::Error;

use crate::persistence::session_store::{
    begin_write, ensure_writable, finish_write, validate_context_key, validate_text, LatchError,
    PutOutcome,
};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Largest artifact in bytes. The schema CHECK repeats it.
pub const ARTIFACT_MAX_BYTES: usize = 16 * 1024;

const FOCUS_ID_MAX_CHARS: usize = 128;
const RUN_ID_MAX_CHARS: usize = 256;

/// Hex characters of the content hash that logs and Debug show.
const HASH_PREFIX_CHARS: usize = 12;

// ---------------------------------------------------------------------------
// Kinds
// ---------------------------------------------------------------------------

/// The closed set of artifact kinds. Held here and not as a SQL CHECK (see
/// outputs_015.sql), so a round-trip test and ALL keep it honest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ArtifactKind {
    /// The structured brief a document is drafted from.
    ContentBrief,
    /// The voice guidance a draft is written in.
    VoiceGuidance,
    /// A validation report over a draft.
    ValidationReport,
    /// The delta items found by session delta validation.
    DeltaItems,
    /// items.id=903: the document a Writing Assistant chat is writing, as the last
    /// pass left it (written by artifact_put, and by artifact_append section by section).
    WorkingDraft,
}

impl ArtifactKind {
    /// The closed set, in documentation order.
    pub const ALL: [ArtifactKind; 5] = [
        ArtifactKind::ContentBrief,
        ArtifactKind::VoiceGuidance,
        ArtifactKind::ValidationReport,
        ArtifactKind::DeltaItems,
        ArtifactKind::WorkingDraft,
    ];

    /// The value stored in the kind column.
    pub fn as_str(self) -> &'static str {
        match self {
            ArtifactKind::ContentBrief => "content_brief",
            ArtifactKind::VoiceGuidance => "voice_guidance",
            ArtifactKind::ValidationReport => "validation_report",
            ArtifactKind::DeltaItems => "delta_items",
            ArtifactKind::WorkingDraft => "working_draft",
        }
    }

    pub fn parse(s: &str) -> Option<ArtifactKind> {
        ArtifactKind::ALL.iter().copied().find(|k| k.as_str() == s)
    }
}

// ---------------------------------------------------------------------------
// Fingerprint (what a log may say about an artifact)
// ---------------------------------------------------------------------------

/// A count and a hash of an artifact's text: all a log or a Debug may carry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fingerprint {
    pub bytes: usize,
    /// The first HASH_PREFIX_CHARS hex characters of SHA-256 of the text.
    pub sha256_prefix: String,
}

impl fmt::Display for Fingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} bytes, sha256 {}", self.bytes, self.sha256_prefix)
    }
}

/// The fingerprint of any artifact text.
pub fn fingerprint(content: &str) -> Fingerprint {
    let digest = Sha256::digest(content.as_bytes());
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    Fingerprint {
        bytes: content.len(),
        sha256_prefix: hex[..HASH_PREFIX_CHARS].to_owned(),
    }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// No variant carries artifact text.
#[derive(Debug, Error)]
pub enum PassArtifactError {
    #[error("Database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("{0}")]
    Validation(String),
    /// The row's version is not the one the writer expected. `current_version`
    /// is None when the row does not exist.
    #[error("pass artifact version conflict (current version {current_version:?})")]
    VersionConflict { current_version: Option<i64> },
    #[error("{0}")]
    Latch(#[from] LatchError),
}

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Clone, PartialEq, Eq)]
pub struct PassArtifact {
    pub context_key: String,
    pub focus_id: String,
    pub kind: ArtifactKind,
    pub content: String,
    pub version: i64,
    pub written_by_run: String,
    pub created_at: String,
    pub updated_at: String,
}

impl PassArtifact {
    pub fn fingerprint(&self) -> Fingerprint {
        fingerprint(&self.content)
    }
}

/// Never prints the text.
impl fmt::Debug for PassArtifact {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PassArtifact")
            .field("kind", &self.kind)
            .field("version", &self.version)
            .field("content", &format_args!("<{}>", self.fingerprint()))
            .field("written_by_run", &self.written_by_run)
            .finish_non_exhaustive()
    }
}

/// One artifact write. `expected_version` is the version the writer read: None
/// means "I expect the artifact to be absent".
#[derive(Clone)]
pub struct ArtifactWrite<'a> {
    pub context_key: &'a str,
    pub focus_id: &'a str,
    pub kind: ArtifactKind,
    pub content: &'a str,
    pub written_by_run: &'a str,
    pub expected_version: Option<i64>,
}

/// Never prints the text.
impl fmt::Debug for ArtifactWrite<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ArtifactWrite")
            .field("kind", &self.kind)
            .field("expected_version", &self.expected_version)
            .field("content", &format_args!("<{}>", fingerprint(self.content)))
            .field("written_by_run", &self.written_by_run)
            .finish_non_exhaustive()
    }
}

/// What clear_artifact did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClearOutcome {
    /// The row existed and was removed.
    Cleared,
    /// There was nothing to remove.
    AlreadyAbsent,
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

fn validate_content(s: &str) -> Result<(), String> {
    if s.trim().is_empty() {
        return Err("artifact content must not be empty".into());
    }
    if s.len() > ARTIFACT_MAX_BYTES {
        return Err(format!(
            "artifact content is {} bytes, the limit is {ARTIFACT_MAX_BYTES}",
            s.len()
        ));
    }
    if s.contains('\0') {
        return Err("artifact content must not contain a NUL character".into());
    }
    Ok(())
}

/// The scope of every read and write: the chat and the Focus.
fn validate_scope(context_key: &str, focus_id: &str) -> Result<(), PassArtifactError> {
    validate_context_key(context_key).map_err(PassArtifactError::Validation)?;
    validate_text("focus_id", focus_id, FOCUS_ID_MAX_CHARS).map_err(PassArtifactError::Validation)
}

// ---------------------------------------------------------------------------
// Rows
// ---------------------------------------------------------------------------

const COLUMNS: &str =
    "context_key, focus_id, kind, content, version, written_by_run, created_at, updated_at";

fn row_to_artifact(r: &sqlx::sqlite::SqliteRow) -> Result<PassArtifact, sqlx::Error> {
    let kind_text: String = r.try_get("kind")?;
    // A stored kind outside the closed set is a corrupt or foreign row. It is
    // an error, never skipped, so a reader fails closed.
    let kind = ArtifactKind::parse(&kind_text).ok_or_else(|| {
        sqlx::Error::Decode(format!("pass_artifacts holds an unknown kind '{kind_text}'").into())
    })?;
    Ok(PassArtifact {
        context_key: r.try_get("context_key")?,
        focus_id: r.try_get("focus_id")?,
        kind,
        content: r.try_get("content")?,
        version: r.try_get("version")?,
        written_by_run: r.try_get("written_by_run")?,
        created_at: r.try_get("created_at")?,
        updated_at: r.try_get("updated_at")?,
    })
}

async fn get_inner(
    conn: &mut SqliteConnection,
    context_key: &str,
    focus_id: &str,
    kind: ArtifactKind,
) -> Result<Option<PassArtifact>, sqlx::Error> {
    let row = sqlx::query(&format!(
        "SELECT {COLUMNS} FROM pass_artifacts
         WHERE context_key = ? AND focus_id = ? AND kind = ?"
    ))
    .bind(context_key)
    .bind(focus_id)
    .bind(kind.as_str())
    .fetch_optional(&mut *conn)
    .await?;
    row.as_ref().map(row_to_artifact).transpose()
}

/// One artifact, or None when it was never written (or was cleared). A read
/// error is an Err: callers must not treat it as absent.
pub async fn get_artifact(
    conn: &mut SqliteConnection,
    context_key: &str,
    focus_id: &str,
    kind: ArtifactKind,
) -> Result<Option<PassArtifact>, PassArtifactError> {
    validate_scope(context_key, focus_id)?;
    Ok(get_inner(conn, context_key, focus_id, kind).await?)
}

// ---------------------------------------------------------------------------
// Writes
// ---------------------------------------------------------------------------

/// Idempotent upsert with version compare-and-set. Refused once the chat is
/// latched. The rules are those of session_store::put_session_variable:
///   - absent row, expected None: created at version 1;
///   - present row, expected equals its version: updated to version + 1, or
///     Unchanged (nothing written) when the text is already the same;
///   - any other combination is VersionConflict, EXCEPT a replay: if the row
///     already holds this exact text and this same run wrote it, the write
///     already happened (a retry after a lost reply) and the result is
///     Unchanged with the row's version.
pub async fn put_artifact(
    conn: &mut SqliteConnection,
    write: &ArtifactWrite<'_>,
) -> Result<PutOutcome, PassArtifactError> {
    validate_scope(write.context_key, write.focus_id)?;
    validate_content(write.content).map_err(PassArtifactError::Validation)?;
    validate_text("written_by_run", write.written_by_run, RUN_ID_MAX_CHARS)
        .map_err(PassArtifactError::Validation)?;
    if matches!(write.expected_version, Some(v) if v < 1) {
        return Err(PassArtifactError::Validation(
            "expected_version must be at least 1".into(),
        ));
    }

    begin_write(conn).await?;
    let result = put_inner(conn, write).await;
    finish_write(conn, result).await
}

async fn put_inner(
    conn: &mut SqliteConnection,
    write: &ArtifactWrite<'_>,
) -> Result<PutOutcome, PassArtifactError> {
    ensure_writable(conn, write.context_key).await?;

    let current = get_inner(conn, write.context_key, write.focus_id, write.kind).await?;
    let now = crate::providers::utils::now();

    let Some(current) = current else {
        if write.expected_version.is_some() {
            return Err(PassArtifactError::VersionConflict {
                current_version: None,
            });
        }
        sqlx::query(&format!(
            "INSERT INTO pass_artifacts ({COLUMNS}) VALUES (?, ?, ?, ?, 1, ?, ?, ?)"
        ))
        .bind(write.context_key)
        .bind(write.focus_id)
        .bind(write.kind.as_str())
        .bind(write.content)
        .bind(write.written_by_run)
        .bind(&now)
        .bind(&now)
        .execute(&mut *conn)
        .await?;
        return Ok(PutOutcome::Created { version: 1 });
    };

    let replay = current.content == write.content && current.written_by_run == write.written_by_run;
    match write.expected_version {
        Some(v) if v == current.version => {
            if current.content == write.content {
                return Ok(PutOutcome::Unchanged {
                    version: current.version,
                });
            }
            let next = current.version + 1;
            let done = sqlx::query(
                "UPDATE pass_artifacts
                 SET content = ?, version = ?, written_by_run = ?, updated_at = ?
                 WHERE context_key = ? AND focus_id = ? AND kind = ? AND version = ?",
            )
            .bind(write.content)
            .bind(next)
            .bind(write.written_by_run)
            .bind(&now)
            .bind(write.context_key)
            .bind(write.focus_id)
            .bind(write.kind.as_str())
            .bind(current.version)
            .execute(&mut *conn)
            .await?;
            if done.rows_affected() != 1 {
                return Err(PassArtifactError::VersionConflict {
                    current_version: Some(current.version),
                });
            }
            Ok(PutOutcome::Updated { version: next })
        }
        _ if replay => Ok(PutOutcome::Unchanged {
            version: current.version,
        }),
        _ => Err(PassArtifactError::VersionConflict {
            current_version: Some(current.version),
        }),
    }
}

/// items.id=888 stage 6: remove every artifact of the chat, every Focus and kind,
/// inside a write transaction the caller holds (persistence::chat_clear, the archive
/// hook). Not latch-gated: it only removes, and a latched chat's artifacts written
/// before the latch go with the chat. Returns how many rows went.
pub(crate) async fn clear_chat_in_tx(
    conn: &mut SqliteConnection,
    context_key: &str,
) -> Result<u64, PassArtifactError> {
    let done = sqlx::query("DELETE FROM pass_artifacts WHERE context_key = ?")
        .bind(context_key)
        .execute(&mut *conn)
        .await?;
    Ok(done.rows_affected())
}

/// items.id=888 stage 7: remove the artifacts last written before `cutoff` (the writers'
/// RFC 3339 text; idx_pass_artifacts_updated_at), inside a write transaction the caller
/// holds (persistence::store_sweep). Not latch-gated: it only removes. Returns how many
/// rows went.
pub(crate) async fn sweep_older_than_in_tx(
    conn: &mut SqliteConnection,
    cutoff: &str,
) -> Result<u64, PassArtifactError> {
    let done = sqlx::query("DELETE FROM pass_artifacts WHERE updated_at < ?")
        .bind(cutoff)
        .execute(&mut *conn)
        .await?;
    Ok(done.rows_affected())
}

/// Remove one artifact of one Focus in one chat, so the next pass finds it
/// ABSENT, as on a fresh chat. The only delete a pass can ask for (the archive
/// hook's clear_chat_in_tx above removes a whole chat). Refused once the
/// chat is latched, inside its own transaction, like every writer here (a
/// latched chat saved nothing to remove). Other kinds, other Focuses, other
/// chats and the session variables, the latch included, are untouched.
pub async fn clear_artifact(
    conn: &mut SqliteConnection,
    context_key: &str,
    focus_id: &str,
    kind: ArtifactKind,
) -> Result<ClearOutcome, PassArtifactError> {
    validate_scope(context_key, focus_id)?;
    begin_write(conn).await?;
    let result = async {
        ensure_writable(conn, context_key).await?;
        let done = sqlx::query(
            "DELETE FROM pass_artifacts WHERE context_key = ? AND focus_id = ? AND kind = ?",
        )
        .bind(context_key)
        .bind(focus_id)
        .bind(kind.as_str())
        .execute(&mut *conn)
        .await?;
        Ok::<ClearOutcome, PassArtifactError>(if done.rows_affected() > 0 {
            ClearOutcome::Cleared
        } else {
            ClearOutcome::AlreadyAbsent
        })
    }
    .await;
    finish_write(conn, result).await
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persistence::session_store::{
        latch_write_suppressed, put_session_variable, test_db::mem_outputs_conn, SessionWrite,
    };

    const CHAT: &str = "chat-1";
    const FOCUS: &str = "writing-assistant";
    /// Distinctive text no error or Debug may ever contain.
    const SECRET: &str = "ZX9-ARTIFACT-SECRET-TEXT";

    fn w<'a>(
        kind: ArtifactKind,
        content: &'a str,
        run: &'a str,
        expected: Option<i64>,
    ) -> ArtifactWrite<'a> {
        ArtifactWrite {
            context_key: CHAT,
            focus_id: FOCUS,
            kind,
            content,
            written_by_run: run,
            expected_version: expected,
        }
    }

    const BRIEF: ArtifactKind = ArtifactKind::ContentBrief;

    async fn count(conn: &mut SqliteConnection) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM pass_artifacts")
            .fetch_one(&mut *conn)
            .await
            .unwrap()
    }

    #[test]
    fn the_kinds_are_the_closed_set_and_round_trip() {
        let names: Vec<&str> = ArtifactKind::ALL.iter().map(|k| k.as_str()).collect();
        assert_eq!(
            names,
            [
                "content_brief",
                "voice_guidance",
                "validation_report",
                "delta_items",
                "working_draft"
            ]
        );
        for k in ArtifactKind::ALL {
            assert_eq!(ArtifactKind::parse(k.as_str()), Some(k));
        }
        for bad in ["", "Content_Brief", "session_intent", "content_brief ", "x"] {
            assert_eq!(ArtifactKind::parse(bad), None, "{bad:?}");
        }
    }

    #[tokio::test]
    async fn create_then_read_back() {
        let mut conn = mem_outputs_conn().await;
        let out = put_artifact(&mut conn, &w(BRIEF, "the brief", "run-a", None))
            .await
            .unwrap();
        assert_eq!(out, PutOutcome::Created { version: 1 });
        let got = get_artifact(&mut conn, CHAT, FOCUS, BRIEF)
            .await
            .unwrap()
            .unwrap();
        assert_eq!((got.content.as_str(), got.version), ("the brief", 1));
        assert_eq!(got.written_by_run, "run-a");
        assert_eq!(got.kind, BRIEF);
        assert!(
            get_artifact(&mut conn, CHAT, FOCUS, ArtifactKind::DeltaItems)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn text_is_stored_exactly_including_braces_and_newlines() {
        let mut conn = mem_outputs_conn().await;
        let text = "{\"items\": [1, 2]}\n  indented \u{e9}\u{1f600}\n";
        put_artifact(&mut conn, &w(ArtifactKind::DeltaItems, text, "run-a", None))
            .await
            .unwrap();
        let got = get_artifact(&mut conn, CHAT, FOCUS, ArtifactKind::DeltaItems)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(got.content, text, "the store never rewrites the text");
    }

    #[tokio::test]
    async fn compare_and_set_accepts_the_current_version_and_refuses_a_stale_one() {
        let mut conn = mem_outputs_conn().await;
        put_artifact(&mut conn, &w(BRIEF, "v1", "run-a", None))
            .await
            .unwrap();
        let out = put_artifact(&mut conn, &w(BRIEF, "v2", "run-a", Some(1)))
            .await
            .unwrap();
        assert_eq!(out, PutOutcome::Updated { version: 2 });

        // A second writer that still holds version 1 loses.
        let err = put_artifact(&mut conn, &w(BRIEF, "v3", "run-b", Some(1)))
            .await
            .unwrap_err();
        assert!(
            matches!(
                err,
                PassArtifactError::VersionConflict {
                    current_version: Some(2)
                }
            ),
            "{err:?}"
        );
        // Expecting absence of a present row is a conflict too.
        let err = put_artifact(&mut conn, &w(BRIEF, "v3", "run-b", None))
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            PassArtifactError::VersionConflict {
                current_version: Some(2)
            }
        ));
        // Expecting a version of an absent row is a conflict.
        let err = put_artifact(
            &mut conn,
            &w(ArtifactKind::VoiceGuidance, "x", "run-b", Some(1)),
        )
        .await
        .unwrap_err();
        assert!(matches!(
            err,
            PassArtifactError::VersionConflict {
                current_version: None
            }
        ));

        let got = get_artifact(&mut conn, CHAT, FOCUS, BRIEF)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            (got.content.as_str(), got.version),
            ("v2", 2),
            "the loser changed nothing"
        );
    }

    #[tokio::test]
    async fn a_replayed_write_is_idempotent_and_does_not_bump_the_version() {
        let mut conn = mem_outputs_conn().await;
        put_artifact(&mut conn, &w(BRIEF, "v1", "run-a", None))
            .await
            .unwrap();
        // The same create again (a retry after a lost reply).
        let again = put_artifact(&mut conn, &w(BRIEF, "v1", "run-a", None))
            .await
            .unwrap();
        assert_eq!(again, PutOutcome::Unchanged { version: 1 });

        put_artifact(&mut conn, &w(BRIEF, "v2", "run-a", Some(1)))
            .await
            .unwrap();
        // The same update again: the row is at version 2, the writer's expected
        // version is stale, but the row holds this run's text.
        let again = put_artifact(&mut conn, &w(BRIEF, "v2", "run-a", Some(1)))
            .await
            .unwrap();
        assert_eq!(again, PutOutcome::Unchanged { version: 2 });

        // A different run writing identical text at the current version is also
        // Unchanged, and nothing moves.
        let same = put_artifact(&mut conn, &w(BRIEF, "v2", "run-b", Some(2)))
            .await
            .unwrap();
        assert_eq!(same, PutOutcome::Unchanged { version: 2 });
        let got = get_artifact(&mut conn, CHAT, FOCUS, BRIEF)
            .await
            .unwrap()
            .unwrap();
        assert_eq!((got.version, got.written_by_run.as_str()), (2, "run-a"));

        // A different run with a stale version and the same text is a conflict:
        // only the SAME run's replay is forgiven.
        let err = put_artifact(&mut conn, &w(BRIEF, "v2", "run-b", Some(1)))
            .await
            .unwrap_err();
        assert!(matches!(err, PassArtifactError::VersionConflict { .. }));
    }

    #[tokio::test]
    async fn each_kind_focus_and_chat_is_its_own_row() {
        let mut conn = mem_outputs_conn().await;
        for k in ArtifactKind::ALL {
            put_artifact(&mut conn, &w(k, k.as_str(), "run-a", None))
                .await
                .unwrap();
        }
        // Another Focus and another chat, same kind.
        put_artifact(
            &mut conn,
            &ArtifactWrite {
                focus_id: "tech-support",
                ..w(BRIEF, "other focus", "run-a", None)
            },
        )
        .await
        .unwrap();
        put_artifact(
            &mut conn,
            &ArtifactWrite {
                context_key: "chat-2",
                ..w(BRIEF, "other chat", "run-a", None)
            },
        )
        .await
        .unwrap();
        assert_eq!(count(&mut conn).await, 7);

        let own = get_artifact(&mut conn, CHAT, FOCUS, BRIEF)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(own.content, "content_brief");
        let other_focus = get_artifact(&mut conn, CHAT, "tech-support", BRIEF)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(other_focus.content, "other focus");
        let other_chat = get_artifact(&mut conn, "chat-2", FOCUS, BRIEF)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(other_chat.content, "other chat");
        // A third scope sees nothing.
        assert!(get_artifact(&mut conn, "chat-3", FOCUS, BRIEF)
            .await
            .unwrap()
            .is_none());
        assert!(get_artifact(&mut conn, CHAT, "quick-ask", BRIEF)
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn the_size_limit_is_exact_and_the_text_rules_hold() {
        let mut conn = mem_outputs_conn().await;
        let exact = "a".repeat(ARTIFACT_MAX_BYTES);
        put_artifact(&mut conn, &w(BRIEF, &exact, "run-a", None))
            .await
            .unwrap();
        let over = "a".repeat(ARTIFACT_MAX_BYTES + 1);
        let err = put_artifact(&mut conn, &w(BRIEF, &over, "run-a", Some(1)))
            .await
            .unwrap_err();
        assert!(matches!(err, PassArtifactError::Validation(_)), "{err:?}");
        // The limit counts bytes, not characters: 8193 two-byte characters.
        let wide = "\u{e9}".repeat(ARTIFACT_MAX_BYTES / 2 + 1);
        assert!(put_artifact(&mut conn, &w(BRIEF, &wide, "run-a", Some(1)))
            .await
            .is_err());
        for bad in ["", "   ", "\n\t", "a\0b"] {
            let err = put_artifact(&mut conn, &w(BRIEF, bad, "run-a", Some(1)))
                .await
                .unwrap_err();
            assert!(
                matches!(err, PassArtifactError::Validation(_)),
                "{bad:?}: {err:?}"
            );
        }
        let got = get_artifact(&mut conn, CHAT, FOCUS, BRIEF)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(got.content, exact, "no refused write changed the row");
    }

    #[tokio::test]
    async fn the_scope_and_version_arguments_are_validated() {
        let mut conn = mem_outputs_conn().await;
        for (key, focus) in [("", FOCUS), (CHAT, ""), (&"k".repeat(129)[..], FOCUS)] {
            let err = put_artifact(
                &mut conn,
                &ArtifactWrite {
                    context_key: key,
                    focus_id: focus,
                    ..w(BRIEF, "x", "run-a", None)
                },
            )
            .await
            .unwrap_err();
            assert!(matches!(err, PassArtifactError::Validation(_)));
            assert!(get_artifact(&mut conn, key, focus, BRIEF).await.is_err());
        }
        assert!(matches!(
            put_artifact(&mut conn, &w(BRIEF, "x", "run-a", Some(0)))
                .await
                .unwrap_err(),
            PassArtifactError::Validation(_)
        ));
        assert!(matches!(
            put_artifact(&mut conn, &w(BRIEF, "x", "", None))
                .await
                .unwrap_err(),
            PassArtifactError::Validation(_)
        ));
        assert_eq!(count(&mut conn).await, 0);
    }

    // -- the one-time latch --------------------------------------------------

    #[tokio::test]
    async fn a_latched_chat_refuses_every_write_and_keeps_what_it_has() {
        let mut conn = mem_outputs_conn().await;
        put_artifact(&mut conn, &w(BRIEF, "before", "run-a", None))
            .await
            .unwrap();
        latch_write_suppressed(&mut conn, CHAT, FOCUS, "run-a")
            .await
            .unwrap();

        let err = put_artifact(&mut conn, &w(BRIEF, "after", "run-a", Some(1)))
            .await
            .unwrap_err();
        assert!(
            matches!(err, PassArtifactError::Latch(LatchError::Suppressed)),
            "{err:?}"
        );
        let err = put_artifact(&mut conn, &w(ArtifactKind::DeltaItems, "x", "run-a", None))
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            PassArtifactError::Latch(LatchError::Suppressed)
        ));
        let err = clear_artifact(&mut conn, CHAT, FOCUS, BRIEF)
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            PassArtifactError::Latch(LatchError::Suppressed)
        ));
        let got = get_artifact(&mut conn, CHAT, FOCUS, BRIEF)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(got.content, "before", "a refused write changed nothing");

        // The latch is scoped to the chat: another chat still writes.
        put_artifact(
            &mut conn,
            &ArtifactWrite {
                context_key: "chat-2",
                ..w(BRIEF, "elsewhere", "run-a", None)
            },
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn an_unreadable_latch_fails_the_write_closed() {
        let mut conn = mem_outputs_conn().await;
        sqlx::query("ALTER TABLE session_variables RENAME TO session_variables_gone")
            .execute(&mut conn)
            .await
            .unwrap();
        let err = put_artifact(&mut conn, &w(BRIEF, "x", "run-a", None))
            .await
            .unwrap_err();
        assert!(
            matches!(err, PassArtifactError::Latch(LatchError::Unreadable(_))),
            "{err:?}"
        );
        let err = clear_artifact(&mut conn, CHAT, FOCUS, BRIEF)
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            PassArtifactError::Latch(LatchError::Unreadable(_))
        ));
        assert_eq!(count(&mut conn).await, 0, "nothing was written");
    }

    // -- clear ---------------------------------------------------------------

    #[tokio::test]
    async fn clear_removes_one_kind_of_one_scope_and_nothing_else() {
        let mut conn = mem_outputs_conn().await;
        for k in ArtifactKind::ALL {
            put_artifact(&mut conn, &w(k, "text", "run-a", None))
                .await
                .unwrap();
        }
        put_artifact(
            &mut conn,
            &ArtifactWrite {
                focus_id: "tech-support",
                ..w(BRIEF, "other focus", "run-a", None)
            },
        )
        .await
        .unwrap();
        put_session_variable(
            &mut conn,
            &SessionWrite {
                context_key: CHAT,
                focus_id: FOCUS,
                name: "locked_intent",
                value: "intent",
                written_by_run: "run-a",
                expected_version: None,
            },
        )
        .await
        .unwrap();

        assert_eq!(
            clear_artifact(&mut conn, CHAT, FOCUS, BRIEF).await.unwrap(),
            ClearOutcome::Cleared
        );
        assert!(get_artifact(&mut conn, CHAT, FOCUS, BRIEF)
            .await
            .unwrap()
            .is_none());
        // A second clear is a no-op.
        assert_eq!(
            clear_artifact(&mut conn, CHAT, FOCUS, BRIEF).await.unwrap(),
            ClearOutcome::AlreadyAbsent
        );
        // Everything else is untouched.
        assert_eq!(count(&mut conn).await, 5);
        let sessions: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM session_variables")
            .fetch_one(&mut conn)
            .await
            .unwrap();
        assert_eq!(sessions, 1);

        // After a clear, a write starts again at version 1 expecting absence.
        let out = put_artifact(&mut conn, &w(BRIEF, "again", "run-b", None))
            .await
            .unwrap();
        assert_eq!(out, PutOutcome::Created { version: 1 });
    }

    // -- privacy: text never reaches a message, a Debug or a log --------------

    #[tokio::test]
    async fn no_error_and_no_debug_carries_the_artifact_text() {
        let mut conn = mem_outputs_conn().await;
        let text = format!("{SECRET} and more text");
        put_artifact(&mut conn, &w(BRIEF, &text, "run-a", None))
            .await
            .unwrap();

        let got = get_artifact(&mut conn, CHAT, FOCUS, BRIEF)
            .await
            .unwrap()
            .unwrap();
        let write = w(BRIEF, &text, "run-b", Some(7));
        let shown = format!("{got:?} {write:?} {}", got.fingerprint());
        assert!(!shown.contains(SECRET), "{shown}");
        assert!(shown.contains("bytes") && shown.contains("sha256"));

        // Every refusal, with the secret as the content.
        let mut messages = Vec::new();
        for result in [
            put_artifact(&mut conn, &w(BRIEF, &text, "run-b", Some(7))).await, // conflict
            put_artifact(&mut conn, &w(BRIEF, &format!("{text}\0"), "run-b", Some(1))).await, // NUL
            put_artifact(
                &mut conn,
                &w(
                    BRIEF,
                    &format!("{SECRET}{}", "a".repeat(ARTIFACT_MAX_BYTES)),
                    "r",
                    Some(1),
                ),
            )
            .await, // too long
        ] {
            let err = result.unwrap_err();
            messages.push(format!("{err} | {err:?}"));
        }
        latch_write_suppressed(&mut conn, CHAT, FOCUS, "run-a")
            .await
            .unwrap();
        let err = put_artifact(&mut conn, &w(BRIEF, &text, "run-b", Some(1)))
            .await
            .unwrap_err();
        messages.push(format!("{err} | {err:?}"));
        for m in &messages {
            assert!(!m.contains(SECRET), "{m}");
        }
    }

    #[test]
    fn the_fingerprint_is_a_count_and_a_short_stable_hash() {
        let a = fingerprint("alpha");
        assert_eq!(a, fingerprint("alpha"));
        assert_ne!(a, fingerprint("alphb"));
        assert_eq!(a.bytes, 5);
        assert_eq!(a.sha256_prefix.len(), HASH_PREFIX_CHARS);
        // SHA-256("alpha") = 8ed3f6ad685b...
        assert_eq!(a.sha256_prefix, "8ed3f6ad685b");
        assert_eq!(fingerprint("\u{e9}").bytes, 2, "bytes, not characters");
    }

    /// The module is the single writer of pass_artifacts and logs nothing. A
    /// source scan, because the privacy rule is about what the code does.
    #[test]
    fn the_module_logs_nothing_and_uses_no_query_macro() {
        let src = include_str!("pass_artifact_store.rs");
        let code = src.split("#[cfg(test)]").next().unwrap();
        for banned in [
            "log::",
            "println!",
            "eprintln!",
            "dbg!",
            "tracing::",
            "sqlx::query!(",
            "query_as!(",
            "query_scalar!(",
        ] {
            assert!(!code.contains(banned), "pass_artifact_store uses {banned}");
        }
        // Three DELETEs: clear_artifact's (one kind, a pass asks for it), the archive
        // hook's clear_chat_in_tx (a whole chat, items.id=888 stage 6) and the 30-day
        // sweep's sweep_older_than_in_tx (items.id=888 stage 7).
        assert_eq!(code.matches("DELETE FROM").count(), 3);
    }

    // -- the schema's own defence --------------------------------------------

    #[tokio::test]
    async fn the_table_itself_refuses_what_the_store_refuses() {
        let mut conn = mem_outputs_conn().await;
        let insert = |content: String, key: &'static str| {
            sqlx::query(
                "INSERT INTO pass_artifacts
                 (context_key, focus_id, kind, content, written_by_run, created_at, updated_at)
                 VALUES (?, 'f', 'content_brief', ?, 'r', 't', 't')",
            )
            .bind(key)
            .bind(content)
        };
        assert!(insert("ok".into(), "k1").execute(&mut conn).await.is_ok());
        assert!(insert(String::new(), "k2")
            .execute(&mut conn)
            .await
            .is_err());
        assert!(insert("a".repeat(ARTIFACT_MAX_BYTES), "k3")
            .execute(&mut conn)
            .await
            .is_ok());
        assert!(insert("a".repeat(ARTIFACT_MAX_BYTES + 1), "k4")
            .execute(&mut conn)
            .await
            .is_err());
        assert!(insert("ok".into(), "").execute(&mut conn).await.is_err());
        // The primary key is the scope.
        assert!(insert("dup".into(), "k1").execute(&mut conn).await.is_err());
    }

    #[tokio::test]
    async fn a_stored_row_of_an_unknown_kind_is_never_returned_as_a_known_one() {
        let mut conn = mem_outputs_conn().await;
        sqlx::query(
            "INSERT INTO pass_artifacts
             (context_key, focus_id, kind, content, written_by_run, created_at, updated_at)
             VALUES (?, ?, 'session_intent', 'x', 'r', 't', 't')",
        )
        .bind(CHAT)
        .bind(FOCUS)
        .execute(&mut conn)
        .await
        .unwrap();
        // The known kinds still read as absent, and the foreign row is never
        // returned as one of them.
        assert!(get_artifact(&mut conn, CHAT, FOCUS, BRIEF)
            .await
            .unwrap()
            .is_none());
    }
}
