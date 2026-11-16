// src-tauri/src/commands/turn_marker.rs
//
// items.id=901 stage 6 (ruling R-D.5 of the approved plan): the DUPLICATE-TURN MARKER of a chat
// send. send_message saves the user's turn BEFORE it starts the run (message_store has no delete),
// and the plan preflight runs before that save, so a plan that needs approval is refused without
// a saved turn. The one gap left is a plan that was approved when the preflight ran and is not
// when the run starts (an approval cleared, the answers changed, a latch set in between): the
// turn is already saved and the send returns `plan_required`. The marker lets the call-again
// recognise that saved turn instead of saving it a second time.
//
// WHERE IT IS SET AND CONSUMED (both in commands::messages::send_message):
//   * SET by remember_unstarted_turn(), only when load_and_authorize_run returns
//     LoadOutcome::PlanRequired AFTER the turn was saved: the saved message id and a SHA-256 of
//     the content, in the memory-only home (conductor::plan_memory). One per chat, a newer one
//     replaces it, it expires after MARKER_TTL (120 s).
//   * CONSUMED by save_user_turn_once(), at the step that used to be the plain save of the
//     turn, which runs AFTER the plan preflight (a call-again whose plan still needs approval
//     is refused by the preflight first and leaves the marker alone). The marker is always taken
//     (removed). The save is skipped only if ALL of these hold: the marker has not expired, the
//     content hash matches, the marked message is still the NEWEST message of the chat, and it
//     is a USER turn (so no assistant row follows it). Any mismatch discards the marker and the
//     turn is saved normally.
//
// NOTHING IS PERSISTED FOR IT. The marker lives only in plan_memory: it is not written to
// messages.db, outputs.db or any file, not under the latch or anywhere else, never logged, never
// serialized, never in a checkpoint. A one-time (latched) chat stays write-free apart from the
// message itself, which this module saves exactly as send_message always did. After an app
// restart the marker is gone and the turn saves once more (a known, accepted behavior).

use crate::conductor::plan_memory::{Marker, PlanMemory};
use crate::persistence::message_store::{self, MessageRecord};

/// Whether a taken marker lets the call-again skip the save: the content is the marked
/// content, and the marked message is the newest of the chat and a user turn.
pub(crate) fn marker_allows_skip(
    marker: &Marker,
    content: &str,
    history: &[MessageRecord],
) -> bool {
    marker.matches(content)
        && history
            .last()
            .is_some_and(|last| last.id == marker.message_id && last.sender == "user")
}

/// Save the user's turn, unless a marker from an earlier send of this same text shows it is
/// already saved (see the module comment). Returns the turn's record either way.
pub(crate) async fn save_user_turn_once(
    plan_memory: &PlanMemory,
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
    context_key: &str,
    content: &str,
) -> Result<MessageRecord, String> {
    // Always taken: a match consumes it, a mismatch discards it.
    if let Some(marker) = plan_memory.take_marker(user_id, persona_id, context_key) {
        let history = message_store::list_messages(user_id, persona_id, key_hex, context_key)
            .await
            .map_err(|e| e.to_string())?;
        if marker_allows_skip(&marker, content, &history) {
            if let Some(saved) = history.into_iter().last() {
                return Ok(saved);
            }
        }
    }
    message_store::save_message(
        user_id,
        persona_id,
        key_hex,
        context_key,
        "user",
        content,
        None,
    )
    .await
    .map_err(|e| e.to_string())
}

/// Leave the marker for a turn that was saved but whose run did not start because the plan
/// needed an approval it no longer has. Memory only.
pub(crate) fn remember_unstarted_turn(
    plan_memory: &PlanMemory,
    user_id: &str,
    persona_id: &str,
    context_key: &str,
    saved: &MessageRecord,
    content: &str,
) {
    plan_memory.set_marker(user_id, persona_id, context_key, &saved.id, content);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conductor::plan_memory::MARKER_TTL;
    use crate::persistence::session_store::{latch_write_suppressed, test_db::mem_outputs_conn};
    use crate::test_support::ENV_MUTEX;
    use std::time::Duration;

    const USER: &str = "marker-user";
    const PERSONA: &str = "marker-persona";
    const CHAT: &str = "chat-1";
    const TEXT: &str = "please write the letter";

    fn key_hex() -> String {
        "0b".repeat(32)
    }

    struct Env {
        _dir: tempfile::TempDir,
        _lock: tokio::sync::MutexGuard<'static, ()>,
        saved_root: Option<String>,
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
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("QR_DATA_ROOT", dir.path());
        crate::persistence::migrations::migrate_messages_db(USER, PERSONA, &key_hex())
            .await
            .expect("messages.db migrates");
        Env {
            _dir: dir,
            _lock: lock,
            saved_root,
        }
    }

    async fn messages() -> Vec<MessageRecord> {
        message_store::list_messages(USER, PERSONA, &key_hex(), CHAT)
            .await
            .unwrap()
    }

    async fn send(mem: &PlanMemory, content: &str) -> MessageRecord {
        save_user_turn_once(mem, USER, PERSONA, &key_hex(), CHAT, content)
            .await
            .unwrap()
    }

    async fn add(sender: &str, content: &str) -> MessageRecord {
        message_store::save_message(USER, PERSONA, &key_hex(), CHAT, sender, content, None)
            .await
            .unwrap()
    }

    /// The race: the preflight passed, the turn was saved, the run's own check found the plan
    /// no longer approved. The marker is left, and the call-again does not save twice.
    #[tokio::test]
    async fn the_race_leaves_a_marker_and_the_call_again_does_not_save_the_turn_twice() {
        let _env = env().await;
        let mem = PlanMemory::default();
        // First send: no marker, the turn is saved; then the run's plan check says
        // PlanRequired and send_message leaves the marker.
        let saved = send(&mem, TEXT).await;
        remember_unstarted_turn(&mem, USER, PERSONA, CHAT, &saved, TEXT);
        assert_eq!(mem.marker_count(), 1);
        assert_eq!(messages().await.len(), 1);

        // The call-again (after approve_run_plan): the SAME turn, not saved again.
        let again = send(&mem, TEXT).await;
        assert_eq!(again.id, saved.id, "the saved turn is reused");
        assert_eq!(messages().await.len(), 1, "one row, not two");
        assert_eq!(mem.marker_count(), 0, "a match consumes the marker");

        // The marker is spent: sending the same words again is a new turn.
        send(&mem, TEXT).await;
        assert_eq!(messages().await.len(), 2);
    }

    #[tokio::test]
    async fn a_different_text_saves_and_discards_the_marker() {
        let _env = env().await;
        let mem = PlanMemory::default();
        let saved = send(&mem, TEXT).await;
        remember_unstarted_turn(&mem, USER, PERSONA, CHAT, &saved, TEXT);
        let other = send(&mem, "something else entirely").await;
        assert_ne!(other.id, saved.id);
        assert_eq!(messages().await.len(), 2);
        assert_eq!(mem.marker_count(), 0, "a mismatch discards it");
    }

    #[tokio::test]
    async fn a_newer_message_or_an_assistant_row_after_the_turn_discards_the_marker() {
        let _env = env().await;
        for newer in [("user", "a newer turn"), ("assistant", "a reply")] {
            let mem = PlanMemory::default();
            let before = messages().await.len();
            let saved = send(&mem, TEXT).await;
            remember_unstarted_turn(&mem, USER, PERSONA, CHAT, &saved, TEXT);
            add(newer.0, newer.1).await;
            // The marked turn is no longer the newest message: it saves normally.
            let again = send(&mem, TEXT).await;
            assert_ne!(again.id, saved.id, "{newer:?}");
            assert_eq!(messages().await.len(), before + 3, "{newer:?}");
            assert_eq!(mem.marker_count(), 0, "{newer:?}");
        }
    }

    #[tokio::test]
    async fn an_expired_marker_is_ignored() {
        let _env = env().await;
        let mem = PlanMemory::default();
        let saved = send(&mem, TEXT).await;
        remember_unstarted_turn(&mem, USER, PERSONA, CHAT, &saved, TEXT);
        mem.age_marker(USER, PERSONA, CHAT, MARKER_TTL + Duration::from_secs(1));
        let again = send(&mem, TEXT).await;
        assert_ne!(again.id, saved.id);
        assert_eq!(messages().await.len(), 2);
    }

    /// The marker belongs to its own user, Persona and chat.
    #[tokio::test]
    async fn another_chat_persona_or_user_never_uses_the_marker() {
        let _env = env().await;
        let mem = PlanMemory::default();
        let saved = send(&mem, TEXT).await;
        remember_unstarted_turn(&mem, USER, PERSONA, CHAT, &saved, TEXT);
        // Another chat's send finds no marker and saves.
        let other_chat = save_user_turn_once(&mem, USER, PERSONA, &key_hex(), "chat-2", TEXT)
            .await
            .unwrap();
        assert_ne!(other_chat.id, saved.id);
        assert_eq!(mem.marker_count(), 1, "the chat-1 marker is untouched");
        // Clearing the chat (archive) removes it.
        mem.clear_chat(USER, PERSONA, CHAT);
        assert_ne!(send(&mem, TEXT).await.id, saved.id);
    }

    /// A latched (one-time) chat works the same, and nothing is written for the marker: the
    /// persona's outputs.db gains no row, and the only new message row is the turn itself.
    #[tokio::test]
    async fn a_latched_chat_still_works_and_writes_nothing_for_the_marker() {
        let _env = env().await;
        let mut outputs = mem_outputs_conn().await;
        latch_write_suppressed(&mut outputs, CHAT, "writing-assistant", "run-a")
            .await
            .unwrap();
        let tables_before = row_totals(&mut outputs).await;

        let mem = PlanMemory::default();
        let saved = send(&mem, TEXT).await;
        remember_unstarted_turn(&mem, USER, PERSONA, CHAT, &saved, TEXT);
        let again = send(&mem, TEXT).await;
        assert_eq!(again.id, saved.id);
        assert_eq!(messages().await.len(), 1);
        // Nothing persisted for the marker: no row of the latched chat's database changed.
        assert_eq!(row_totals(&mut outputs).await, tables_before);
    }

    async fn row_totals(conn: &mut sqlx::SqliteConnection) -> Vec<(String, i64)> {
        let names: Vec<String> = sqlx::query_scalar(
            "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
        )
        .fetch_all(&mut *conn)
        .await
        .unwrap();
        let mut out = Vec::new();
        for n in names {
            let c: i64 = sqlx::query_scalar(&format!("SELECT COUNT(*) FROM \"{n}\""))
                .fetch_one(&mut *conn)
                .await
                .unwrap();
            out.push((n, c));
        }
        out
    }

    /// The rule itself, over hand-built histories (no database).
    #[test]
    fn the_skip_rule_needs_the_hash_the_newest_message_and_a_user_turn() {
        let mem = PlanMemory::default();
        mem.set_marker(USER, PERSONA, CHAT, "m-1", TEXT);
        let marker = mem.take_marker(USER, PERSONA, CHAT).unwrap();
        let rec = |id: &str, sender: &str| MessageRecord {
            id: id.to_owned(),
            context_key: CHAT.to_owned(),
            sender: sender.to_owned(),
            content: String::new(),
            focus_run_id: None,
            created_at: "t".to_owned(),
            is_error: false,
            personal_flagged: false,
            provider_id: None,
            model_id: None,
            reply_status: message_store::ReplyStatus::Complete,
            outbound_content: None,
        };
        assert!(marker_allows_skip(
            &marker,
            TEXT,
            &[rec("m-0", "user"), rec("m-1", "user")]
        ));
        assert!(!marker_allows_skip(&marker, "other", &[rec("m-1", "user")]));
        assert!(!marker_allows_skip(
            &marker,
            TEXT,
            &[rec("m-1", "user"), rec("m-2", "user")]
        ));
        assert!(!marker_allows_skip(
            &marker,
            TEXT,
            &[rec("m-1", "user"), rec("m-2", "assistant")]
        ));
        assert!(!marker_allows_skip(
            &marker,
            TEXT,
            &[rec("m-1", "assistant")]
        ));
        assert!(!marker_allows_skip(&marker, TEXT, &[rec("m-9", "user")]));
        assert!(!marker_allows_skip(&marker, TEXT, &[]));
    }

    fn code_of(src: &str) -> String {
        src.split("\n#[cfg(test)]\nmod tests {")
            .next()
            .unwrap()
            .to_owned()
    }

    /// The marker never reaches a persisted store or a log: this module writes only the user's
    /// own turn through message_store::save_message, logs nothing, and the marker calls exist
    /// nowhere else in the crate; send_message calls the two helpers in the reviewed places.
    #[test]
    fn the_marker_never_reaches_the_persisted_store_or_a_log() {
        let code = code_of(include_str!("turn_marker.rs"));
        let live: String = code
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        for banned in [
            "log::",
            "println!",
            "eprintln!",
            "dbg!",
            "tracing::",
            "sqlx",
            "output_store",
            "session_store",
            "plan_approval_store",
            "run_park_store",
            "std::fs",
            "serde",
        ] {
            assert!(!live.contains(banned), "'{banned}' in turn_marker.rs");
        }
        // The only persistent write is the user's turn itself, exactly as before.
        assert_eq!(live.matches("message_store::save_message(").count(), 1);
        assert_eq!(live.matches("set_marker(").count(), 1);
        assert_eq!(live.matches("take_marker(").count(), 1);

        // Crate-wide: no other file sets or takes a marker.
        fn walk(dir: std::path::PathBuf, f: &mut dyn FnMut(&str, &str)) {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    walk(path, f);
                } else if path.extension().is_some_and(|e| e == "rs") {
                    f(
                        &path.to_string_lossy(),
                        &std::fs::read_to_string(&path).unwrap(),
                    );
                }
            }
        }
        let mut users = Vec::new();
        walk(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
            &mut |name, text| {
                if name.ends_with("conductor/plan_memory.rs")
                    || name.ends_with("_tests.rs")
                    || name.ends_with("commands/turn_marker.rs")
                {
                    return;
                }
                let code = code_of(text);
                if code.contains(".set_marker(") || code.contains(".take_marker(") {
                    users.push(name.to_owned());
                }
            },
        );
        assert!(users.is_empty(), "{users:?}");

        // send_message: the helper replaces the plain save, after the preflight, and the
        // marker is set at the PlanRequired arm, after the save.
        let msgs = code_of(include_str!("messages.rs"));
        let send = msgs.split("pub async fn send_message").nth(1).unwrap();
        let send = send
            .split("pub(crate) async fn start_reply")
            .next()
            .unwrap();
        let preflight = send.find("execution::plan_preflight(").unwrap();
        let once = send.find("turn_marker::save_user_turn_once(").unwrap();
        let load = send.find("execution::load_and_authorize_run(").unwrap();
        let remember = send.find("turn_marker::remember_unstarted_turn(").unwrap();
        assert!(preflight < once && once < load && load < remember);
        assert!(
            !send.contains("message_store::save_message("),
            "the plain save is gone from send_message"
        );
        assert_eq!(
            send.matches("turn_marker::remember_unstarted_turn(")
                .count(),
            1
        );
        // And the marker is set only in the PlanRequired arm.
        let arm = send[remember.saturating_sub(400)..remember].to_owned();
        assert!(arm.contains("PlanRequired"));
    }
}
