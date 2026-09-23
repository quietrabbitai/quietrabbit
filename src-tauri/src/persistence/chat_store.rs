// src-tauri/src/persistence/chat_store.rs
//
// Persona-scoped chat history for messages.db — decisions.id=739/740
// (items.id=384 slice 6). Lives in the same per-user, per-persona
// SQLCipher-encrypted messages.db as message_store.rs (schema:
// messages_002.sql's `chats` table + messages.chat_id column), reusing
// that module's own DB opener rather than duplicating it (see
// message_store::open_messages_db's own doc comment on why).
//
// Backs commands/chats.rs, which in turn backs the persona-scoped
// chat-history list/switcher UI (items.id=384 slice 7).
//
// context_key for a chat is always "chat-{id}" -- a NEW transcript
// identity distinct from the pre-existing "persona-hub-*"/"tier3-access-*"
// strings ChatPane already uses. Those older context keys have no `chats`
// row and no `chat_id` on their messages -- this module does not touch
// them, and list_chats never surfaces them.
//
// items.id=546: chats are created LAZILY, not eagerly. There used to be a
// standalone create_chat() called the instant a user picked a persona in
// the picker, before any message existed -- that let empty, message-less
// chats accumulate and be indistinguishable from real ones in History, and
// nothing ever bumped last_message_at/title after creation either. Both
// problems share one fix: ensure_chat_and_bump_activity() below is called
// from commands/messages.rs::send_message, once, right before the user's
// turn is persisted -- it creates the row on the first message under a
// given context_key and bumps activity/sets title-once on every later one.
// A context_key with no chats row simply has no chat yet (or never will,
// for the legacy flat pseudo-conversations) -- there is nothing to list
// until a real message actually sends under a "chat-*" context_key.
//
// QUERY STYLE: runtime sqlx::query() only — no query!() macros, matching
// message_store.rs (D6-346's many-small-encrypted-DB topology has no
// static DATABASE_URL for query! to check against).

use sqlx::Row;
use thiserror::Error;

use crate::persistence::message_store::{self, MessageStoreError};

// ---------------------------------------------------------------------------
// ChatRecord
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct ChatRecord {
    pub id: String,
    pub persona_id: String,
    pub context_key: String,
    pub title: Option<String>,
    pub archived_at: Option<String>,
    pub created_at: String,
    pub last_message_at: String,
}

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum ChatStoreError {
    #[error("Database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error(transparent)]
    MessagesDb(#[from] MessageStoreError),
}

// ---------------------------------------------------------------------------
// Row mapping helper
// ---------------------------------------------------------------------------

fn row_to_chat_record(r: &sqlx::sqlite::SqliteRow) -> Result<ChatRecord, sqlx::Error> {
    Ok(ChatRecord {
        id: r.try_get("id")?,
        persona_id: r.try_get("persona_id")?,
        context_key: r.try_get("context_key")?,
        title: r.try_get("title")?,
        archived_at: r.try_get("archived_at")?,
        created_at: r.try_get("created_at")?,
        last_message_at: r.try_get("last_message_at")?,
    })
}

// ---------------------------------------------------------------------------
// Write
// ---------------------------------------------------------------------------

/// items.id=546: creates the owning `chats` row on the FIRST message sent
/// under `context_key` (a chats row no longer exists eagerly from a mere
/// persona switch -- CloudChatAccessPane's handleStartNewChat now only
/// mints a fresh "chat-{uuid}" context_key client-side, never calls an
/// eager create-chat command), and bumps last_message_at/sets title-once
/// on every later message. One statement, one round trip -- deliberately
/// not a separate create-then-update pair, so "does this chat exist yet"
/// is never a race between this and a later send under the same
/// context_key.
///
/// Only touches `context_key`s that look like a real chat (the "chat-"
/// prefix, this module's own established convention) -- the legacy flat
/// "tier3-access-{persona_id}"/"persona-hub-*" pseudo-conversations must
/// keep having no chats row at all (see this module's header comment), or
/// they'd start silently appearing in History the first time anyone sends
/// a message from the app's default, un-switched state.
///
/// Returns Ok(true) if a chats row now exists for context_key (created or
/// bumped), Ok(false) if context_key isn't chat-shaped and nothing was
/// touched. Caller (commands/messages.rs::send_message) treats Err as
/// non-fatal -- the message itself still saves under context_key even if
/// this fails; the chat just won't have a `chats` row (won't appear in
/// History) until a later successful send retries this for the same key.
pub async fn ensure_chat_and_bump_activity(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
    context_key: &str,
    title_candidate: Option<&str>,
) -> Result<bool, ChatStoreError> {
    if !context_key.starts_with("chat-") {
        return Ok(false);
    }

    let mut conn = message_store::open_messages_db(user_id, persona_id, key_hex).await?;
    let id = uuid::Uuid::new_v4().to_string();
    let timestamp = crate::providers::utils::now();

    sqlx::query(
        "INSERT INTO chats
         (id, persona_id, context_key, title, archived_at, created_at, last_message_at)
         VALUES (?, ?, ?, ?, NULL, ?, ?)
         ON CONFLICT(context_key) DO UPDATE SET
             last_message_at = excluded.last_message_at,
             title = COALESCE(chats.title, excluded.title)",
    )
    .bind(&id)
    .bind(persona_id)
    .bind(context_key)
    .bind(title_candidate)
    .bind(&timestamp)
    .bind(&timestamp)
    .execute(&mut conn)
    .await?;

    Ok(true)
}

// ---------------------------------------------------------------------------
// Read
// ---------------------------------------------------------------------------

/// List a Persona's non-archived chats, most-recent-first by
/// last_message_at -- decisions.id=739's own "default sort
/// most-recent-first" framing. Archived chats are excluded outright, not
/// just sorted last: no UI in this item's scope shows them (a future
/// "view archived" affordance, if built, gets its own query rather than a
/// flag on this one).
pub async fn list_chats(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
) -> Result<Vec<ChatRecord>, ChatStoreError> {
    let mut conn = message_store::open_messages_db(user_id, persona_id, key_hex).await?;

    let rows = sqlx::query(
        "SELECT id, persona_id, context_key, title, archived_at, created_at, last_message_at
         FROM chats
         WHERE persona_id = ? AND archived_at IS NULL
         ORDER BY last_message_at DESC",
    )
    .bind(persona_id)
    .fetch_all(&mut conn)
    .await?;

    let mut out = Vec::with_capacity(rows.len());
    for r in &rows {
        out.push(row_to_chat_record(r)?);
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Update
// ---------------------------------------------------------------------------

/// Archive a chat. decisions.id=739: archiving is the only lifecycle
/// transition this item builds -- explicit delete stays a distinct,
/// separately-tracked future action, never implicit or bundled here.
/// Scoped by `persona_id` as well as `chat_id` in the WHERE clause: same
/// ownership-by-WHERE-clause discipline commands/active_board.rs's own
/// update_topic_state already documents (the per-scope encrypted DB
/// topology is the real ownership boundary; this is defense in depth
/// against a chat_id from a different persona's own row set matching by
/// accident within the same messages.db).
pub async fn archive_chat(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
    chat_id: &str,
) -> Result<(), ChatStoreError> {
    let mut conn = message_store::open_messages_db(user_id, persona_id, key_hex).await?;
    let timestamp = crate::providers::utils::now();

    sqlx::query("UPDATE chats SET archived_at = ? WHERE id = ? AND persona_id = ?")
        .bind(&timestamp)
        .bind(chat_id)
        .bind(persona_id)
        .execute(&mut conn)
        .await?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::ENV_MUTEX;

    const USER_ID: &str = "user-chat-test";
    const PERSONA_ID: &str = "persona-chat-test";
    const OTHER_PERSONA_ID: &str = "persona-chat-test-other";
    const KEY_HEX: &str = "deadbeef00112233445566778899aabbccddeeff00112233445566778899aa";

    struct TestEnv {
        _tempdir: tempfile::TempDir,
        _lock: tokio::sync::MutexGuard<'static, ()>,
        saved_root: Option<String>,
    }

    impl Drop for TestEnv {
        fn drop(&mut self) {
            match &self.saved_root {
                Some(v) => std::env::set_var("QR_DATA_ROOT", v),
                None => std::env::remove_var("QR_DATA_ROOT"),
            }
        }
    }

    async fn setup() -> TestEnv {
        let lock = ENV_MUTEX.lock().await;
        let saved_root = std::env::var("QR_DATA_ROOT").ok();

        let tempdir = tempfile::tempdir().expect("failed to create tempdir");
        std::env::set_var("QR_DATA_ROOT", tempdir.path());

        crate::persistence::migrations::migrate_messages_db(USER_ID, PERSONA_ID, KEY_HEX)
            .await
            .expect("messages.db migration must succeed in test setup");

        TestEnv {
            _tempdir: tempdir,
            _lock: lock,
            saved_root,
        }
    }

    #[tokio::test]
    async fn ensure_chat_and_bump_activity_creates_a_row_on_first_call() {
        let _env = setup().await;

        let touched = ensure_chat_and_bump_activity(
            USER_ID,
            PERSONA_ID,
            KEY_HEX,
            "chat-round-trip",
            Some("first title"),
        )
        .await
        .expect("ensure_chat_and_bump_activity must succeed");
        assert!(touched, "a chat-shaped context_key must create a row");

        let chats = list_chats(USER_ID, PERSONA_ID, KEY_HEX)
            .await
            .expect("list_chats must succeed");

        assert_eq!(chats.len(), 1);
        assert_eq!(chats[0].context_key, "chat-round-trip");
        assert_eq!(chats[0].title.as_deref(), Some("first title"));
        assert_eq!(chats[0].archived_at, None);
    }

    #[tokio::test]
    async fn ensure_chat_and_bump_activity_bumps_and_preserves_title_on_second_call() {
        let _env = setup().await;

        ensure_chat_and_bump_activity(
            USER_ID,
            PERSONA_ID,
            KEY_HEX,
            "chat-bump",
            Some("original title"),
        )
        .await
        .expect("first call must succeed");

        // Force a real ordering difference the same way a real later
        // message would, rather than asserting on possibly-equal
        // now()-derived timestamps (second-resolution).
        let mut conn = message_store::open_messages_db(USER_ID, PERSONA_ID, KEY_HEX)
            .await
            .expect("open_messages_db must succeed");
        sqlx::query("UPDATE chats SET last_message_at = ? WHERE context_key = ?")
            .bind("2026-09-01T00:00:01Z")
            .bind("chat-bump")
            .execute(&mut conn)
            .await
            .expect("update must succeed");

        let touched = ensure_chat_and_bump_activity(
            USER_ID,
            PERSONA_ID,
            KEY_HEX,
            "chat-bump",
            Some("a later message's title -- must not win"),
        )
        .await
        .expect("second call must succeed");
        assert!(touched);

        let chats = list_chats(USER_ID, PERSONA_ID, KEY_HEX)
            .await
            .expect("list_chats must succeed");

        assert_eq!(chats.len(), 1);
        assert_eq!(
            chats[0].title.as_deref(),
            Some("original title"),
            "title must be set once, from the first message, never overwritten"
        );
        assert_ne!(
            chats[0].last_message_at, "2026-09-01T00:00:01Z",
            "last_message_at must be bumped by the second call"
        );
    }

    #[tokio::test]
    async fn ensure_chat_and_bump_activity_is_a_noop_for_a_non_chat_context_key() {
        let _env = setup().await;

        let touched = ensure_chat_and_bump_activity(
            USER_ID,
            PERSONA_ID,
            KEY_HEX,
            "tier3-access-persona-chat-test",
            Some("should never be used"),
        )
        .await
        .expect("must not error for a non-chat-shaped context_key");
        assert!(
            !touched,
            "a non-chat-shaped context_key must not create a row"
        );

        let chats = list_chats(USER_ID, PERSONA_ID, KEY_HEX)
            .await
            .expect("list_chats must succeed");
        assert!(
            chats.is_empty(),
            "no row should exist for a non-chat context_key"
        );
    }

    #[tokio::test]
    async fn list_chats_sorts_most_recent_first() {
        let _env = setup().await;

        ensure_chat_and_bump_activity(USER_ID, PERSONA_ID, KEY_HEX, "chat-first", None)
            .await
            .expect("ensure_chat_and_bump_activity must succeed");
        ensure_chat_and_bump_activity(USER_ID, PERSONA_ID, KEY_HEX, "chat-second", None)
            .await
            .expect("ensure_chat_and_bump_activity must succeed");

        // Two chats created back-to-back can legitimately share the same
        // now()-derived last_message_at timestamp (second-resolution) --
        // force a real ordering difference the same way a real later
        // message would, rather than asserting on possibly-equal
        // timestamps.
        let mut conn = message_store::open_messages_db(USER_ID, PERSONA_ID, KEY_HEX)
            .await
            .expect("open_messages_db must succeed");
        sqlx::query("UPDATE chats SET last_message_at = ? WHERE context_key = ?")
            .bind("2026-09-01T00:00:01Z")
            .bind("chat-first")
            .execute(&mut conn)
            .await
            .expect("update must succeed");
        sqlx::query("UPDATE chats SET last_message_at = ? WHERE context_key = ?")
            .bind("2026-09-01T00:00:02Z")
            .bind("chat-second")
            .execute(&mut conn)
            .await
            .expect("update must succeed");

        let chats = list_chats(USER_ID, PERSONA_ID, KEY_HEX)
            .await
            .expect("list_chats must succeed");

        assert_eq!(chats.len(), 2);
        assert_eq!(
            chats[0].context_key, "chat-second",
            "most recent last_message_at first"
        );
        assert_eq!(chats[1].context_key, "chat-first");
    }

    #[tokio::test]
    async fn list_chats_excludes_archived() {
        let _env = setup().await;

        ensure_chat_and_bump_activity(USER_ID, PERSONA_ID, KEY_HEX, "chat-archive-me", None)
            .await
            .expect("ensure_chat_and_bump_activity must succeed");

        let chats = list_chats(USER_ID, PERSONA_ID, KEY_HEX)
            .await
            .expect("list_chats must succeed");
        let chat_id = chats[0].id.clone();

        archive_chat(USER_ID, PERSONA_ID, KEY_HEX, &chat_id)
            .await
            .expect("archive_chat must succeed");

        let chats = list_chats(USER_ID, PERSONA_ID, KEY_HEX)
            .await
            .expect("list_chats must succeed");

        assert!(chats.is_empty(), "archived chat must not be listed");
    }

    #[tokio::test]
    async fn list_chats_scopes_by_persona_id() {
        let _env = setup().await;

        ensure_chat_and_bump_activity(USER_ID, PERSONA_ID, KEY_HEX, "chat-mine", None)
            .await
            .expect("ensure_chat_and_bump_activity must succeed");

        // A different persona_id sharing the same messages.db row set
        // (this test inserts directly rather than via
        // ensure_chat_and_bump_activity, since that function's own DB path
        // is keyed by persona_id -- this exercises list_chats' WHERE
        // persona_id = ? scoping directly against a row that's physically
        // present but belongs to another persona).
        let mut conn = message_store::open_messages_db(USER_ID, PERSONA_ID, KEY_HEX)
            .await
            .expect("open_messages_db must succeed");
        sqlx::query(
            "INSERT INTO chats (id, persona_id, context_key, created_at, last_message_at)
             VALUES ('other-chat', ?, 'chat-other', '2026-09-01T00:00:00Z', '2026-09-01T00:00:00Z')",
        )
        .bind(OTHER_PERSONA_ID)
        .execute(&mut conn)
        .await
        .expect("insert must succeed");

        let chats = list_chats(USER_ID, PERSONA_ID, KEY_HEX)
            .await
            .expect("list_chats must succeed");

        assert_eq!(chats.len(), 1, "must not include the other persona's chat");
        assert_ne!(chats[0].id, "other-chat");
    }

    #[tokio::test]
    async fn archive_chat_does_not_affect_a_different_persona_id() {
        let _env = setup().await;

        let mut conn = message_store::open_messages_db(USER_ID, PERSONA_ID, KEY_HEX)
            .await
            .expect("open_messages_db must succeed");
        sqlx::query(
            "INSERT INTO chats (id, persona_id, context_key, created_at, last_message_at)
             VALUES ('other-chat', ?, 'chat-other', '2026-09-01T00:00:00Z', '2026-09-01T00:00:00Z')",
        )
        .bind(OTHER_PERSONA_ID)
        .execute(&mut conn)
        .await
        .expect("insert must succeed");

        // Attempt to archive it while scoped to the WRONG persona_id.
        archive_chat(USER_ID, PERSONA_ID, KEY_HEX, "other-chat")
            .await
            .expect("archive_chat must not error even if it matches nothing");

        let row = sqlx::query("SELECT archived_at FROM chats WHERE id = 'other-chat'")
            .fetch_one(&mut conn)
            .await
            .expect("query failed");
        let archived_at: Option<String> = row.try_get("archived_at").unwrap();
        assert_eq!(
            archived_at, None,
            "archive_chat scoped to the wrong persona_id must not archive another persona's chat"
        );
    }

    #[tokio::test]
    async fn messages_table_accepts_chat_id() {
        // Confirms messages_002.sql's ALTER TABLE landed and existing
        // context_key-based rows still work with chat_id left NULL.
        let _env = setup().await;

        ensure_chat_and_bump_activity(USER_ID, PERSONA_ID, KEY_HEX, "chat-msg-id-test", None)
            .await
            .expect("ensure_chat_and_bump_activity must succeed");
        let chats = list_chats(USER_ID, PERSONA_ID, KEY_HEX)
            .await
            .expect("list_chats must succeed");
        let chat_id = chats[0].id.clone();

        let saved = message_store::save_message(
            USER_ID,
            PERSONA_ID,
            KEY_HEX,
            "chat-msg-id-test",
            "user",
            "hello",
            None,
            None,
        )
        .await
        .expect("save_message must succeed");

        let mut conn = message_store::open_messages_db(USER_ID, PERSONA_ID, KEY_HEX)
            .await
            .expect("open_messages_db must succeed");
        sqlx::query("UPDATE messages SET chat_id = ? WHERE id = ?")
            .bind(&chat_id)
            .bind(&saved.id)
            .execute(&mut conn)
            .await
            .expect("chat_id column must exist and accept a value");

        let row = sqlx::query("SELECT chat_id FROM messages WHERE id = ?")
            .bind(&saved.id)
            .fetch_one(&mut conn)
            .await
            .expect("query failed");
        let chat_id_col: Option<String> = row.try_get("chat_id").unwrap();
        assert_eq!(chat_id_col.as_deref(), Some(chat_id.as_str()));
    }
}
