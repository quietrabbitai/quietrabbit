// src-tauri/src/persistence/launch_answer_store.rs
//
// items.id=897 stage 3 (decisions.id=948 points 2 and 3): the typed store of a chat's
// LAUNCH ANSWERS, in the persona's encrypted outputs.db as session variables
// (schema/outputs_013.sql) under the six launch names (conductor::bound_spec,
// Source::Launch). No new table.
//
// WRITES. The only writer is put_answers(), and it takes a ValidatedAnswers, a value
// only conductor::launch_config::validate_submission() can build: every question id
// and option checked against the shipped data, no free text, run_change parsed as a
// SettingsPatch. So nothing stores an unvalidated answer. All the answers of one
// submit are written in ONE `BEGIN IMMEDIATE` transaction (session_store's helpers),
// so a submit is all or nothing, even when the one-time latch is set while it runs.
//
// THE LATCH (decisions.id=939 section 4, decisions.id=948 point 3). The one-time
// latch is obeyed with no exemption: a latched chat refuses the whole submit with
// PutError::OneTimeChat (its own code, one_time_chat), and a latch that cannot be
// read refuses it too (LatchUnreadable, fail closed). Answers stored BEFORE the latch
// was set stay in session_variables (setting the latch deletes nothing), keep being
// read by read_answers() (reads are not latch-gated), and so keep seeding later
// passes of that chat. No retention sweep or clear_chat exists yet, so they stay
// until items.id=888 builds one (session_variables rows are covered by it).
//
// READS. read_answers() returns the value of each launch name that has a row, by
// name. A read error is an error, never an empty map.
//
// NOTHING HERE LOGS A VALUE. The values are ids and a settings patch, never user
// content, but the module follows the store convention and logs none.

use std::collections::BTreeMap;

use sqlx::SqliteConnection;

use crate::conductor::bound_spec::LAUNCH_NAMES;
use crate::conductor::launch_config::ValidatedAnswers;
use crate::persistence::session_store::{
    begin_write, ensure_writable, finish_write, get_session_variable, validate_context_key,
    LatchError, SessionStoreError,
};

/// The run id recorded with a launch answer: a submit belongs to no run.
pub const WRITTEN_BY: &str = "launch_answers";

const FOCUS_ID_MAX_CHARS: usize = 128;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PutError {
    /// The chat is a one-time session. The user sees the defaults apply.
    #[error("this chat is a one-time session, so launch answers are not saved")]
    OneTimeChat,
    /// The latch could not be read, so nothing was saved (fail closed).
    #[error("the one-time session latch could not be read, so nothing was saved: {0}")]
    LatchUnreadable(String),
    #[error("the launch answers could not be saved: {0}")]
    Store(String),
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("the launch answers could not be read: {0}")]
pub struct ReadError(pub String);

fn check_keys(context_key: &str, focus_id: &str) -> Result<(), String> {
    validate_context_key(context_key)?;
    if focus_id.is_empty() || focus_id.chars().count() > FOCUS_ID_MAX_CHARS {
        return Err("focus_id must be 1 to 128 characters".to_owned());
    }
    Ok(())
}

/// Save the answers of one submit, all or nothing. Refused whole when the chat is
/// latched or its latch cannot be read. Writing an answer that is already stored
/// leaves its row untouched (nothing changes, no version bump).
pub async fn put_answers(
    conn: &mut SqliteConnection,
    context_key: &str,
    focus_id: &str,
    answers: &ValidatedAnswers,
) -> Result<(), PutError> {
    check_keys(context_key, focus_id).map_err(PutError::Store)?;
    if answers.writes().is_empty() {
        return Ok(());
    }
    begin_write(conn)
        .await
        .map_err(|e| PutError::Store(e.to_string()))?;
    let result = put_inner(conn, context_key, focus_id, answers).await;
    finish_write(conn, result).await
}

async fn put_inner(
    conn: &mut SqliteConnection,
    context_key: &str,
    focus_id: &str,
    answers: &ValidatedAnswers,
) -> Result<(), PutError> {
    ensure_writable(conn, context_key)
        .await
        .map_err(|e| match e {
            LatchError::Suppressed => PutError::OneTimeChat,
            LatchError::Unreadable(m) => PutError::LatchUnreadable(m),
        })?;
    let now = crate::providers::utils::now();
    for (name, value) in answers.writes() {
        sqlx::query(
            "INSERT INTO session_variables
               (context_key, focus_id, name, value, version, written_by_run, created_at, updated_at)
             VALUES (?, ?, ?, ?, 1, ?, ?, ?)
             ON CONFLICT (context_key, focus_id, name) DO UPDATE
               SET value = excluded.value,
                   version = session_variables.version + 1,
                   written_by_run = excluded.written_by_run,
                   updated_at = excluded.updated_at
               WHERE session_variables.value <> excluded.value",
        )
        .bind(context_key)
        .bind(focus_id)
        .bind(*name)
        .bind(value)
        .bind(WRITTEN_BY)
        .bind(&now)
        .bind(&now)
        .execute(&mut *conn)
        .await
        .map_err(|e| PutError::Store(e.to_string()))?;
    }
    Ok(())
}

impl From<sqlx::Error> for PutError {
    fn from(e: sqlx::Error) -> Self {
        PutError::Store(e.to_string())
    }
}

/// The stored value of each launch name of one Focus in one chat, by name. Works in
/// a latched chat (the latch refuses writes only). A name with no row is absent.
pub async fn read_answers(
    conn: &mut SqliteConnection,
    context_key: &str,
    focus_id: &str,
) -> Result<BTreeMap<String, String>, ReadError> {
    check_keys(context_key, focus_id).map_err(ReadError)?;
    let mut out = BTreeMap::new();
    for name in LAUNCH_NAMES {
        let row = get_session_variable(conn, context_key, focus_id, name)
            .await
            .map_err(|e: SessionStoreError| ReadError(e.to_string()))?;
        if let Some(v) = row {
            out.insert(name.to_owned(), v.value);
        }
    }
    Ok(out)
}

/// read_answers() on the persona's outputs.db, opened here (the run-start step has
/// no connection of its own). It opens WITH THE RETRY (output_store::
/// open_outputs_db_retrying, commit 8d8d66f measured lock failures on the plain
/// opener under contention), because a failed read here fails the run start closed.
pub async fn read_answers_for_run(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
    context_key: &str,
    focus_id: &str,
) -> Result<BTreeMap<String, String>, ReadError> {
    let mut conn =
        crate::persistence::output_store::open_outputs_db_retrying(user_id, persona_id, key_hex)
            .await
            .map_err(|e| ReadError(e.to_string()))?;
    read_answers(&mut conn, context_key, focus_id).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conductor::launch_config::{
        validate_submission, LaunchConfig, LaunchOption, LaunchQuestion, ModeProfile, QuestionKind,
    };
    use crate::conductor::model_block::config::EnvelopeOverride;
    use crate::conductor::model_block::document_type::registry;
    use crate::persistence::session_store::test_db::mem_outputs_conn;
    use crate::persistence::session_store::{latch_write_suppressed, write_mode, WriteMode};

    const CHAT: &str = "chat-1";
    const FOCUS: &str = "fx";

    fn opt(id: &str) -> LaunchOption {
        LaunchOption {
            id: id.to_owned(),
            sort_order: 0,
            label: "L".to_owned(),
            hint: String::new(),
        }
    }

    fn config() -> LaunchConfig {
        let q = |id: &str, slot: &str, kind, opts: &[&str]| LaunchQuestion {
            id: id.to_owned(),
            slot: slot.to_owned(),
            kind,
            sort_order: 0,
            asked: true,
            default_option: opts[0].to_owned(),
            prompt: "P".to_owned(),
            options: opts.iter().map(|o| opt(o)).collect(),
        };
        LaunchConfig {
            focus_id: FOCUS.to_owned(),
            questions: vec![
                q(
                    "how",
                    "launch_mode",
                    QuestionKind::Mode,
                    &["quick", "careful"],
                ),
                q(
                    "kind",
                    "document_type",
                    QuestionKind::Choice,
                    &["rma_letter", "cover_letter"],
                ),
            ],
            profiles: ["quick", "careful"]
                .iter()
                .map(|o| ModeProfile {
                    option_id: (*o).to_owned(),
                    envelope: EnvelopeOverride::default(),
                })
                .collect(),
        }
    }

    fn answers(pairs: &[(&str, &str)], run_change: Option<&str>) -> ValidatedAnswers {
        validate_submission(&config(), pairs, run_change, registry().unwrap()).unwrap()
    }

    #[tokio::test]
    async fn answers_round_trip_and_a_second_submit_replaces_them() {
        let mut conn = mem_outputs_conn().await;
        assert!(read_answers(&mut conn, CHAT, FOCUS)
            .await
            .unwrap()
            .is_empty());
        put_answers(
            &mut conn,
            CHAT,
            FOCUS,
            &answers(
                &[("how", "quick"), ("kind", "rma_letter")],
                Some(r#"{"tone":"warm"}"#),
            ),
        )
        .await
        .unwrap();
        let got = read_answers(&mut conn, CHAT, FOCUS).await.unwrap();
        assert_eq!(got["launch_mode"], "quick");
        assert_eq!(got["document_type"], "rma_letter");
        assert_eq!(got["run_change"], r#"{"tone":"warm"}"#);
        // A second submit replaces a value, bumps its version, and leaves an
        // unchanged value's row alone.
        put_answers(
            &mut conn,
            CHAT,
            FOCUS,
            &answers(&[("how", "careful"), ("kind", "rma_letter")], None),
        )
        .await
        .unwrap();
        let got = read_answers(&mut conn, CHAT, FOCUS).await.unwrap();
        assert_eq!(got["launch_mode"], "careful");
        let versions: Vec<(String, i64)> = sqlx::query_as(
            "SELECT name, version FROM session_variables
              WHERE context_key = ? AND focus_id = ? ORDER BY name",
        )
        .bind(CHAT)
        .bind(FOCUS)
        .fetch_all(&mut conn)
        .await
        .unwrap();
        assert_eq!(
            versions,
            [
                ("document_type".to_owned(), 1),
                ("launch_mode".to_owned(), 2),
                ("run_change".to_owned(), 1)
            ]
        );
        // Another chat and another Focus see nothing.
        assert!(read_answers(&mut conn, "other", FOCUS)
            .await
            .unwrap()
            .is_empty());
        assert!(read_answers(&mut conn, CHAT, "fy")
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn a_latched_chat_refuses_the_whole_submit_with_its_own_error_and_writes_nothing() {
        let mut conn = mem_outputs_conn().await;
        latch_write_suppressed(&mut conn, CHAT, FOCUS, "run-1")
            .await
            .unwrap();
        let e = put_answers(
            &mut conn,
            CHAT,
            FOCUS,
            &answers(&[("how", "quick"), ("kind", "rma_letter")], None),
        )
        .await
        .unwrap_err();
        assert_eq!(e, PutError::OneTimeChat);
        assert!(read_answers(&mut conn, CHAT, FOCUS)
            .await
            .unwrap()
            .is_empty());
        // Still one-time: the latch is untouched.
        assert_eq!(
            write_mode(&mut conn, CHAT).await.unwrap(),
            WriteMode::Suppressed
        );
    }

    #[tokio::test]
    async fn answers_stored_before_the_latch_stay_and_are_still_read() {
        let mut conn = mem_outputs_conn().await;
        put_answers(
            &mut conn,
            CHAT,
            FOCUS,
            &answers(&[("how", "careful")], None),
        )
        .await
        .unwrap();
        latch_write_suppressed(&mut conn, CHAT, FOCUS, "run-1")
            .await
            .unwrap();
        // The latch deleted nothing and reads are not latch-gated.
        let got = read_answers(&mut conn, CHAT, FOCUS).await.unwrap();
        assert_eq!(got["launch_mode"], "careful");
        // A later submit is refused whole and cannot change it.
        let e = put_answers(&mut conn, CHAT, FOCUS, &answers(&[("how", "quick")], None))
            .await
            .unwrap_err();
        assert_eq!(e, PutError::OneTimeChat);
        assert_eq!(
            read_answers(&mut conn, CHAT, FOCUS).await.unwrap()["launch_mode"],
            "careful"
        );
    }

    #[tokio::test]
    async fn an_unreadable_latch_fails_the_submit_closed_and_writes_nothing() {
        let mut conn = mem_outputs_conn().await;
        // Break the table the latch is read from: the read fails, so the submit does.
        sqlx::query("ALTER TABLE session_variables RENAME TO session_variables_gone")
            .execute(&mut conn)
            .await
            .unwrap();
        let e = put_answers(&mut conn, CHAT, FOCUS, &answers(&[("how", "quick")], None))
            .await
            .unwrap_err();
        assert!(matches!(e, PutError::LatchUnreadable(_)), "{e:?}");
        // The transaction was undone: the connection is usable again.
        sqlx::query("ALTER TABLE session_variables_gone RENAME TO session_variables")
            .execute(&mut conn)
            .await
            .unwrap();
        assert!(read_answers(&mut conn, CHAT, FOCUS)
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn a_submit_is_all_or_nothing() {
        let mut conn = mem_outputs_conn().await;
        // Make the second write fail (a CHECK on the value length), after the first
        // succeeded inside the same transaction: nothing may remain.
        let v = answers(&[("how", "quick"), ("kind", "rma_letter")], None);
        sqlx::query(
            "CREATE TRIGGER fail_second BEFORE INSERT ON session_variables
             WHEN NEW.name = 'document_type'
             BEGIN SELECT RAISE(ABORT, 'boom'); END",
        )
        .execute(&mut conn)
        .await
        .unwrap();
        let e = put_answers(&mut conn, CHAT, FOCUS, &v).await.unwrap_err();
        assert!(matches!(e, PutError::Store(_)), "{e:?}");
        assert!(read_answers(&mut conn, CHAT, FOCUS)
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn keys_are_checked_and_an_empty_submit_writes_nothing() {
        let mut conn = mem_outputs_conn().await;
        assert!(matches!(
            put_answers(&mut conn, "", FOCUS, &answers(&[("how", "quick")], None)).await,
            Err(PutError::Store(_))
        ));
        assert!(matches!(
            put_answers(&mut conn, CHAT, "", &answers(&[("how", "quick")], None)).await,
            Err(PutError::Store(_))
        ));
        put_answers(&mut conn, CHAT, FOCUS, &answers(&[], None))
            .await
            .unwrap();
        assert!(read_answers(&mut conn, CHAT, FOCUS)
            .await
            .unwrap()
            .is_empty());
        assert!(read_answers(&mut conn, "", FOCUS).await.is_err());
    }

    #[test]
    fn nothing_here_logs_a_value_and_the_module_is_the_only_launch_writer() {
        let text = include_str!("launch_answer_store.rs");
        let body = &text[..text.find("#[cfg(test)]").unwrap()];
        for banned in ["log::", "println!", "eprintln!", "dbg!", "tracing::"] {
            assert!(!body.contains(banned), "{banned}");
        }
        // put_answers takes a ValidatedAnswers only: no other public writer.
        assert_eq!(body.matches("pub async fn put_").count(), 1);
    }
}
