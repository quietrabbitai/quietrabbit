// src-tauri/src/commands/handoff.rs
//
// items.id=387 (S4, decisions.id=942 point 7): the READ-ONLY state of a manual
// hand-off, for the hand-off panel (items.id=912, designed under items.id=900).
//
//   get_handoff_state(persona_id, context_key, focus_id)
//       whether a hand-off is waiting for its reply (the opaque id and when it
//       began), whether a reply has come back (its H- handle, its size and when it
//       was stored), and whether the chat is a one-time session. It NEVER returns
//       the text of a reply: the panel shows size and time, and a pass reads the
//       text by handle (persistence::handoff_draft_store).
//
// THERE IS NO WRITE COMMAND HERE, on purpose. The hand-off is driven by chat passes:
//   * the starter is a pass's reply, copied with the existing "Copy last response"
//     button through the copy-time review (decisions.id=846), so there is no
//     export command and no second gate;
//   * the pasted reply is sent as the NEXT chat turn through send_message, so there
//     is no paste command either;
//   * an abandoned hand-off is cleared by a pass (a guarded typed row), never by a
//     command that writes session_variables, which would be a second write path.
// Whether to offer a cancel control is items.id=900's design.
//
// The same shape as the consent and device commands: writer_gate::command first
// (a key rotation waits for it), the user and key from the unlocked session
// (session_user_and_key: no user_id argument to spoof), the Persona id must be a
// canonical UUID that belongs to that user, and the work is in a *_for_session body
// with no Tauri state, tested directly.
//
// ERROR CODES (ERROR_CODES, each with a placeholder key handoff.error.<code> in the
// frontend's en.json, a test requires it): persona_not_found, invalid_focus_id,
// invalid_context_key, handoff_state_unavailable. The last is any store failure or a
// stored value that fails its validator, and carries no detail.
//
// COLLECTION CONSENT. In a focus_level Focus without the current consent the return
// writer (handoff_ingest) is SILENTLY SKIPPED by the consent gate, so a reply row
// can still say "received" when nothing was saved. This command reports what is
// stored, not what the consent state would allow: the panel reads the consent state
// from get_collection_consent_state beside it, and a Focus author guards the reply
// on consent state (CLAUDE.md, "Manual hand-off").

use serde::Serialize;
use specta::Type;
use tauri::State;

use crate::auth::registry::KeyRegistry;
use crate::commands::persona::session_user_and_key;
use crate::conductor::bound_spec::{self, HANDOFF_PENDING, HANDOFF_RECEIVED};
use crate::persistence::handles::{self, HandleError};
use crate::persistence::{handoff_draft_store, output_store, persona_store, session_store};

/// Every error string this module returns.
pub const ERROR_CODES: [&str; 4] = [
    "persona_not_found",
    "invalid_focus_id",
    "invalid_context_key",
    "handoff_state_unavailable",
];

/// A hand-off whose starter was shown and whose reply has not come back.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Type)]
pub struct HandoffPending {
    /// The opaque hand-off id (`h_` and 12 hex characters). Not secret, not an
    /// internal id.
    pub id: String,
    /// When the hand-off was marked pending (the session variable's updated_at).
    pub since: String,
}

/// A reply that came back and is stored as a Library draft. Never its text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Type)]
pub struct HandoffReceived {
    /// The H- handle a pass reads the reply by.
    pub handle: String,
    /// The size of the stored reply in bytes.
    pub byte_count: u32,
    /// When it was stored.
    pub received_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Type)]
pub struct HandoffState {
    pub pending: Option<HandoffPending>,
    /// None when no reply has come back, or the stored reply was deleted from the
    /// Library since.
    pub received: Option<HandoffReceived>,
    /// The chat is a one-time session: nothing is saved in it, so a hand-off in it
    /// shows its starter and can take no reply back.
    pub one_time_chat: bool,
}

fn unavailable<E: std::fmt::Display>(what: &str, e: E) -> String {
    // The error is a store or validation message, never a stored value.
    log::warn!("get_handoff_state: {what}: {e}");
    "handoff_state_unavailable".to_owned()
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

#[tauri::command]
#[specta::specta]
pub async fn get_handoff_state(
    persona_id: String,
    context_key: String,
    focus_id: String,
    key_registry: State<'_, KeyRegistry>,
    pool: State<'_, sqlx::SqlitePool>,
) -> Result<HandoffState, String> {
    let _lease = crate::auth::writer_gate::command(&key_registry).await?;
    let (user_id, key_hex) = session_user_and_key(&key_registry).await?;
    get_handoff_state_for_session(
        &pool,
        &user_id,
        &key_hex,
        &persona_id,
        &context_key,
        &focus_id,
    )
    .await
}

pub(crate) async fn get_handoff_state_for_session(
    pool: &sqlx::SqlitePool,
    user_id: &str,
    key_hex: &str,
    persona_id: &str,
    context_key: &str,
    focus_id: &str,
) -> Result<HandoffState, String> {
    require_persona(pool, user_id, persona_id).await?;
    require_focus_id(focus_id)?;
    session_store::validate_context_key(context_key)
        .map_err(|_| "invalid_context_key".to_owned())?;

    let mut conn = output_store::open_outputs_db(user_id, persona_id, key_hex)
        .await
        .map_err(|e| unavailable("open outputs.db", e))?;

    let one_time_chat = matches!(
        session_store::write_mode(&mut conn, context_key).await,
        Ok(session_store::WriteMode::Suppressed)
    );

    // A stored value that fails its validator is a store problem, never shown.
    let pending =
        session_store::get_session_variable(&mut conn, context_key, focus_id, HANDOFF_PENDING)
            .await
            .map_err(|e| unavailable("read handoff_pending", e))?;
    let pending = match pending {
        None => None,
        Some(v) => {
            let valid = bound_spec::spec(HANDOFF_PENDING).is_some_and(|s| (s.validate)(&v.value));
            if !valid {
                return Err(unavailable("handoff_pending", "invalid stored value"));
            }
            Some(HandoffPending {
                id: v.value,
                since: v.updated_at,
            })
        }
    };

    let received =
        session_store::get_session_variable(&mut conn, context_key, focus_id, HANDOFF_RECEIVED)
            .await
            .map_err(|e| unavailable("read handoff_received", e))?;
    let received = match received {
        None => None,
        Some(v) => {
            let valid = bound_spec::spec(HANDOFF_RECEIVED).is_some_and(|s| (s.validate)(&v.value));
            if !valid {
                return Err(unavailable("handoff_received", "invalid stored value"));
            }
            // The reply may have been deleted from the Library since: then there is
            // nothing to point at, and it is reported as not received.
            match handles::resolve_draft_handle(&mut conn, focus_id, &v.value).await {
                Ok(output_id) => {
                    match handoff_draft_store::draft_info(&mut conn, focus_id, &output_id).await {
                        Ok(info) => Some(HandoffReceived {
                            handle: v.value,
                            byte_count: u32::try_from(info.byte_count).unwrap_or(u32::MAX),
                            received_at: info.created_at,
                        }),
                        Err(handoff_draft_store::DraftError::NotFound) => None,
                        Err(e) => return Err(unavailable("read the stored reply's size", e)),
                    }
                }
                Err(HandleError::NotFound(_)) | Err(HandleError::Ambiguous(_)) => None,
                Err(e) => return Err(unavailable("resolve the reply handle", e)),
            }
        }
    };

    Ok(HandoffState {
        pending,
        received,
        one_time_chat,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::registry::key_hex;
    use crate::conductor::handoff_render::mark_pending;
    use crate::persistence::session_store::SessionWrite;
    use crate::test_support::ENV_MUTEX;

    const FOCUS: &str = "writing-assistant";
    const CHAT: &str = "chat-hs";
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
        let user = add_user(&pool, "Handoff User").await;
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

    async fn state(e: &Env, chat: &str, focus: &str) -> Result<HandoffState, String> {
        get_handoff_state_for_session(&e.pool, &e.user, &e.key, &e.persona, chat, focus).await
    }

    async fn conn(e: &Env) -> sqlx::SqliteConnection {
        output_store::open_outputs_db(&e.user, &e.persona, &e.key)
            .await
            .expect("outputs.db")
    }

    #[tokio::test]
    async fn a_fresh_chat_has_no_hand_off() {
        let e = env().await;
        assert_eq!(
            state(&e, CHAT, FOCUS).await.unwrap(),
            HandoffState {
                pending: None,
                received: None,
                one_time_chat: false
            }
        );
    }

    #[tokio::test]
    async fn a_pending_hand_off_shows_its_id_and_time_and_no_reply() {
        let e = env().await;
        let mut c = conn(&e).await;
        mark_pending(&mut c, CHAT, FOCUS, "run-1", "offer")
            .await
            .unwrap();
        let s = state(&e, CHAT, FOCUS).await.unwrap();
        let p = s.pending.expect("pending");
        assert_eq!(
            p.id,
            crate::conductor::handoff_render::handoff_id("run-1", "offer")
        );
        assert!(!p.since.is_empty());
        assert!(s.received.is_none() && !s.one_time_chat);
        // Another chat and another Focus see nothing.
        assert!(state(&e, "other-chat", FOCUS)
            .await
            .unwrap()
            .pending
            .is_none());
        assert!(state(&e, CHAT, "other-focus")
            .await
            .unwrap()
            .pending
            .is_none());
    }

    #[tokio::test]
    async fn a_received_reply_shows_handle_size_and_time_and_never_the_text() {
        let e = env().await;
        let mut c = conn(&e).await;
        mark_pending(&mut c, CHAT, FOCUS, "run-1", "offer")
            .await
            .unwrap();
        let secret = "the-secret-pasted-reply é";
        let output_id =
            handoff_draft_store::store_draft(&e.user, &e.persona, &e.key, FOCUS, secret)
                .await
                .unwrap();
        let handle = handles::draft_handle(FOCUS, &output_id);
        session_store::settle_handoff_return(&mut c, CHAT, FOCUS, &handle, "run-2")
            .await
            .unwrap();

        let s = state(&e, CHAT, FOCUS).await.unwrap();
        assert!(s.pending.is_none());
        let r = s.received.clone().expect("received");
        assert_eq!(r.handle, handle);
        assert_eq!(r.byte_count as usize, secret.len());
        assert!(!r.received_at.is_empty());
        // Nothing the command returns, in any form, carries the text.
        assert!(!format!("{s:?}").contains("secret"));
        assert!(!serde_json::to_string(&s).unwrap().contains("secret"));
    }

    #[tokio::test]
    async fn a_reply_deleted_from_the_library_is_reported_as_not_received() {
        let e = env().await;
        let mut c = conn(&e).await;
        mark_pending(&mut c, CHAT, FOCUS, "run-1", "offer")
            .await
            .unwrap();
        let output_id =
            handoff_draft_store::store_draft(&e.user, &e.persona, &e.key, FOCUS, "a reply")
                .await
                .unwrap();
        session_store::settle_handoff_return(
            &mut c,
            CHAT,
            FOCUS,
            &handles::draft_handle(FOCUS, &output_id),
            "run-2",
        )
        .await
        .unwrap();
        assert!(state(&e, CHAT, FOCUS).await.unwrap().received.is_some());
        output_store::delete_output(&e.user, &e.persona, &e.key, &output_id, None)
            .await
            .unwrap();
        let s = state(&e, CHAT, FOCUS).await.unwrap();
        assert!(s.received.is_none(), "{s:?}");
    }

    #[tokio::test]
    async fn a_latched_chat_is_reported_as_one_time() {
        let e = env().await;
        let mut c = conn(&e).await;
        session_store::latch_write_suppressed(&mut c, CHAT, FOCUS, "r0")
            .await
            .unwrap();
        let s = state(&e, CHAT, FOCUS).await.unwrap();
        assert!(s.one_time_chat);
        assert!(!state(&e, "other-chat", FOCUS).await.unwrap().one_time_chat);
    }

    #[tokio::test]
    async fn an_invalid_stored_value_is_unavailable_and_shows_nothing() {
        let e = env().await;
        let mut c = conn(&e).await;
        for (name, value) in [
            (HANDOFF_PENDING, "not-an-id"),
            (HANDOFF_RECEIVED, "not-a-handle"),
        ] {
            session_store::put_session_variable(
                &mut c,
                &SessionWrite {
                    context_key: CHAT,
                    focus_id: FOCUS,
                    name,
                    value,
                    written_by_run: "r",
                    expected_version: None,
                },
            )
            .await
            .unwrap();
            let err = state(&e, CHAT, FOCUS).await.unwrap_err();
            assert_eq!(err, "handoff_state_unavailable");
            sqlx::query("DELETE FROM session_variables WHERE name = ?")
                .bind(name)
                .execute(&mut c)
                .await
                .unwrap();
        }
    }

    #[tokio::test]
    async fn the_inputs_are_checked_and_every_error_is_a_documented_code() {
        let e = env().await;
        let other_user = add_user(&e.pool, "Someone Else").await;
        let results = [
            // Not a UUID, a non-canonical UUID, a UUID the user is not in.
            get_handoff_state_for_session(&e.pool, &e.user, &e.key, "persona-1", CHAT, FOCUS).await,
            get_handoff_state_for_session(
                &e.pool,
                &e.user,
                &e.key,
                &e.persona.to_uppercase(),
                CHAT,
                FOCUS,
            )
            .await,
            get_handoff_state_for_session(&e.pool, &other_user, &e.key, &e.persona, CHAT, FOCUS)
                .await,
            // Bad Focus and chat keys.
            state(&e, CHAT, "").await,
            state(&e, CHAT, "Has Space").await,
            state(&e, CHAT, &"a".repeat(65)).await,
            state(&e, "", FOCUS).await,
            state(&e, &"c".repeat(129), FOCUS).await,
            state(&e, "has\0nul", FOCUS).await,
        ];
        let expect = [
            "persona_not_found",
            "persona_not_found",
            "persona_not_found",
            "invalid_focus_id",
            "invalid_focus_id",
            "invalid_focus_id",
            "invalid_context_key",
            "invalid_context_key",
            "invalid_context_key",
        ];
        for (r, want) in results.iter().zip(expect) {
            let err = r.as_ref().unwrap_err();
            assert_eq!(err, want);
            assert!(ERROR_CODES.contains(&err.as_str()));
        }
    }

    /// Every error code has a placeholder key in the frontend's en.json (Chat-BRAND
    /// owns the wording).
    #[test]
    fn every_error_code_has_a_placeholder_key_in_en_json() {
        let json: serde_json::Value =
            serde_json::from_str(include_str!("../../../frontend/src/i18n/en.json")).unwrap();
        for code in ERROR_CODES {
            let text = json["handoff"]["error"][code]
                .as_str()
                .unwrap_or_else(|| panic!("en.json has no handoff.error.{code}"));
            assert!(!text.trim().is_empty(), "{code}");
        }
        // And no key without a code.
        let keys: Vec<&String> = json["handoff"]["error"]
            .as_object()
            .unwrap()
            .keys()
            .collect();
        assert_eq!(keys.len(), ERROR_CODES.len(), "{keys:?}");
    }

    /// The command is read-only and never reads a reply's text: its production code
    /// names no writer and no text reader.
    #[test]
    fn this_module_only_reads_and_never_touches_a_replys_text() {
        let src = include_str!("handoff.rs");
        let body = src.split("#[cfg(test)]").next().unwrap();
        let code: String = body
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        for banned in [
            "read_draft(",
            "read_draft_by_handle",
            "store_draft",
            "put_session_variable",
            "settle_handoff_return",
            "clear_session_state",
            "latch_write_suppressed",
            "INSERT",
            "UPDATE ",
            "DELETE",
            "persist_ingested_document",
            "get_ingested_document_bytes",
            "SELECT content",
        ] {
            assert!(!code.contains(banned), "handoff.rs names {banned}");
        }
    }
}
