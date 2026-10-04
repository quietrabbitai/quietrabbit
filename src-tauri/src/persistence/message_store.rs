// src-tauri/src/persistence/message_store.rs
//
// Chat/transcript message persistence for messages.db — per-user, per-persona,
// SQLCipher encrypted. Path: /users/{user_id}/personas/{persona_id}/messages.db
//
// Backs commands/messages.rs (send_message/list_messages), which in turn
// backs ChatPane.tsx -- the real component behind MiddleZone's chatPane prop
// for both Persona hub chat and CloudChatAccessPane's starter-drafting pane.
//
// QUERY STYLE: runtime sqlx::query() only — no query!() macros.
// PRAGMA key applied via SqliteConnectOptions (D6-346).
// Caller supplies bare hex; store wraps it in SQLCipher x'...' syntax.

use std::path::PathBuf;

use sqlx::ConnectOptions;
use sqlx::Row;
use sqlx::SqliteConnection;
use thiserror::Error;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const VALID_SENDER: &[&str] = &["user", "assistant"];

// ---------------------------------------------------------------------------
// MessageRecord
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct MessageRecord {
    pub id: String,
    pub context_key: String,
    pub sender: String,
    pub content: String,
    pub focus_run_id: Option<String>,
    pub created_at: String,
    /// items.id=587 (messages_004.sql): true for a placeholder backfilled
    /// with a plain-language failure message rather than a real reply --
    /// build_conversation_prompt (commands/messages.rs) skips these, the
    /// same way it already skips a still-empty placeholder, so a failure
    /// is never replayed back into the model's own conversation history.
    pub is_error: bool,
}

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum MessageStoreError {
    #[error("Database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("Validation error: {0}")]
    Validation(String),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Migration error: {0}")]
    Migration(#[from] crate::persistence::migrations::MigrationError),
}

// ---------------------------------------------------------------------------
// Path helper
// ---------------------------------------------------------------------------

pub(crate) fn get_messages_db_path(user_id: &str, persona_id: &str) -> PathBuf {
    crate::persistence::migrations::get_data_root()
        .join("users")
        .join(user_id)
        .join("personas")
        .join(persona_id)
        .join("messages.db")
}

// ---------------------------------------------------------------------------
// DB opener
// ---------------------------------------------------------------------------

/// Open messages.db with SQLCipher key.
/// Caller supplies bare hex; store wraps it in SQLCipher x'...' syntax.
/// PRAGMA key fires before journal_mode via SqliteConnectOptions (D6-346).
/// busy_timeout=5000ms guards against transient SQLITE_BUSY during concurrent
/// UI reads and sends.
///
/// Rejects an empty key_hex up front with a typed Validation error instead of
/// letting SQLCipher fail on it — the frontend never has a real key_hex to
/// supply yet (Layer 8 auth unbuilt; see ChatPane.tsx), so this is defense-
/// in-depth for any future caller that reaches here without one, not
/// something this item's own code paths are expected to trigger.
///
/// pub(crate), not private: items.id=384 slice 6's chat_store.rs (the
/// `chats` table, messages_002.sql) lives in this same messages.db and
/// reuses this opener rather than duplicating the SQLCipher-open sequence
/// -- one opener for one physical database, matching CLAUDE.md's own
/// emphasis on this exact invariant (PRAGMA key before journal_mode) not
/// being something to risk two copies drifting apart on.
pub(crate) async fn open_messages_db(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
) -> Result<SqliteConnection, MessageStoreError> {
    if key_hex.is_empty() {
        return Err(MessageStoreError::Validation(
            "key_hex required".to_string(),
        ));
    }

    let db_path = get_messages_db_path(user_id, persona_id);

    // BUG FOUND + FIXED (2026-09-01 live verification pass, items.id=384
    // slice 7): this used to be `if !db_path.exists()`, which only ever
    // ran migrations against a brand-new file. That was silently correct
    // as long as "messages" had exactly one schema version ever (there
    // was nothing pending for an existing file to miss) -- but it stopped
    // being correct the moment messages_002.sql (the `chats` table) landed:
    // every messages.db created before that file existed now opens
    // forever stuck on schema v1, `chats` never created, `list_chats`
    // failing with "no such table: chats" on real, pre-existing user data.
    // Confirmed live against this dev machine's own test account.
    //
    // Fix: always call migrate_messages_db, relying on run_migrations'
    // own idempotent/pending-only behavior (schema_version-tracked,
    // already exercised by e.g. migrate_personal_db_is_idempotent_on_real_file)
    // rather than a file-existence guess about whether anything is
    // pending. The same `if !db_path.exists()` pattern was ALSO present in
    // personal_store.rs's open_personal_db -- fixed separately as
    // items.id=389, same pattern applied there.
    crate::persistence::migrations::migrate_messages_db(user_id, persona_id, key_hex).await?;

    let conn = crate::providers::utils::connect_options_encrypted(&db_path, key_hex)
        .create_if_missing(false)
        .pragma("busy_timeout", "5000")
        .connect()
        .await?;

    Ok(conn)
}

// ---------------------------------------------------------------------------
// Row mapping helper
// ---------------------------------------------------------------------------

fn row_to_message_record(r: &sqlx::sqlite::SqliteRow) -> Result<MessageRecord, sqlx::Error> {
    Ok(MessageRecord {
        id: r.try_get("id")?,
        context_key: r.try_get("context_key")?,
        sender: r.try_get("sender")?,
        content: r.try_get("content")?,
        focus_run_id: r.try_get("focus_run_id")?,
        created_at: r.try_get("created_at")?,
        is_error: r.try_get::<i64, _>("is_error")? != 0,
    })
}

// ---------------------------------------------------------------------------
// Write
// ---------------------------------------------------------------------------

/// Persist one message (a user turn or an assistant turn) to messages.db.
/// Returns the saved record.
#[allow(clippy::too_many_arguments)] // Explicit architecture boundary; see D6-342/D6-346.
pub async fn save_message(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
    context_key: &str,
    sender: &str,
    content: &str,
    focus_run_id: Option<&str>,
) -> Result<MessageRecord, MessageStoreError> {
    if !VALID_SENDER.contains(&sender) {
        return Err(MessageStoreError::Validation(format!(
            "Invalid sender '{}'. Must be one of: {}",
            sender,
            VALID_SENDER.join(", ")
        )));
    }
    let id = uuid::Uuid::new_v4().to_string();
    let timestamp = crate::providers::utils::now();
    let mut conn = open_messages_db(user_id, persona_id, key_hex).await?;

    sqlx::query(
        "INSERT INTO messages
         (id, context_key, sender, content, focus_run_id, created_at)
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(context_key)
    .bind(sender)
    .bind(content)
    .bind(focus_run_id)
    .bind(&timestamp)
    .execute(&mut conn)
    .await?;

    Ok(MessageRecord {
        id,
        context_key: context_key.to_owned(),
        sender: sender.to_owned(),
        content: content.to_owned(),
        focus_run_id: focus_run_id.map(|s| s.to_owned()),
        created_at: timestamp,
        is_error: false,
    })
}

// ---------------------------------------------------------------------------
// Read
// ---------------------------------------------------------------------------

/// List every message for a context_key, oldest first — the transcript
/// display/fetch order. Doubles as "get transcript" (commands/messages.rs's
/// list_messages IPC command) — no separate get_transcript store fn.
pub async fn list_messages(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
    context_key: &str,
) -> Result<Vec<MessageRecord>, MessageStoreError> {
    let mut conn = open_messages_db(user_id, persona_id, key_hex).await?;

    let rows = sqlx::query(
        "SELECT id, context_key, sender, content, focus_run_id, created_at, is_error
         FROM messages
         WHERE context_key = ?
         ORDER BY created_at ASC",
    )
    .bind(context_key)
    .fetch_all(&mut conn)
    .await?;

    let mut out = Vec::with_capacity(rows.len());
    for r in &rows {
        out.push(row_to_message_record(r).map_err(MessageStoreError::Database)?);
    }
    Ok(out)
}

/// Fetch a single message by id — the read a command needs
/// before mutating it. Returns None if not found (an
/// Option return, not a NotFound error variant, since "not found" is a
/// normal caller-checkable condition here, matching
/// focus_settings_store::get_focus_settings's own shape).
pub async fn get_message(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
    message_id: &str,
) -> Result<Option<MessageRecord>, MessageStoreError> {
    let mut conn = open_messages_db(user_id, persona_id, key_hex).await?;

    let row = sqlx::query(
        "SELECT id, context_key, sender, content, focus_run_id, created_at, is_error
         FROM messages
         WHERE id = ?",
    )
    .bind(message_id)
    .fetch_optional(&mut conn)
    .await?;

    match row {
        None => Ok(None),
        Some(r) => Ok(Some(
            row_to_message_record(&r).map_err(MessageStoreError::Database)?,
        )),
    }
}

/// Fetch the assistant placeholder row owned by a given focus run.
/// items.id=587: the generic lookup `finalize_chat_reply`
/// (commands/messages.rs) uses so every place a run can finish or resume
/// (send_message's own backfill, resume_run, scheduled_sweep.rs) can find
/// the right message to update by `focus_run_id` alone -- none of them need
/// to separately track/pass the message_id or context_key a send started
/// with. `focus_run_id` is set exactly once, on the single assistant
/// placeholder `save_message` reserves per run (commands/messages.rs's
/// send_message, step 4) and never reused across runs, so at most one row
/// ever matches. None means this run_id has no owning chat message at all
/// -- e.g. a submit_focus_run/Board-originated run -- which callers treat
/// as a safe no-op, not an error.
pub async fn find_assistant_message_by_focus_run_id(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
    focus_run_id: &str,
) -> Result<Option<MessageRecord>, MessageStoreError> {
    let mut conn = open_messages_db(user_id, persona_id, key_hex).await?;

    let row = sqlx::query(
        "SELECT id, context_key, sender, content, focus_run_id, created_at, is_error
         FROM messages
         WHERE focus_run_id = ? AND sender = 'assistant'",
    )
    .bind(focus_run_id)
    .fetch_optional(&mut conn)
    .await?;

    match row {
        None => Ok(None),
        Some(r) => Ok(Some(
            row_to_message_record(&r).map_err(MessageStoreError::Database)?,
        )),
    }
}

// ---------------------------------------------------------------------------
// Update
// ---------------------------------------------------------------------------

/// Backfill a placeholder assistant message's content once its focus run
/// finishes generating, pausing, or resuming (commands::messages::
/// finalize_chat_reply calls this). `is_error` (items.id=587, messages_004.sql)
/// marks a plain-language failure message rather than a real reply, so
/// build_conversation_prompt (commands/messages.rs) never replays it back
/// into the model's own conversation history on a later send.
pub async fn update_message_content(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
    message_id: &str,
    content: &str,
    is_error: bool,
) -> Result<(), MessageStoreError> {
    let mut conn = open_messages_db(user_id, persona_id, key_hex).await?;

    sqlx::query("UPDATE messages SET content = ?, is_error = ? WHERE id = ?")
        .bind(content)
        .bind(is_error)
        .bind(message_id)
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
    use crate::persistence::migrations::parse_statements;
    use sqlx::sqlite::SqliteConnectOptions;
    use sqlx::Connection;

    const MESSAGES_SCHEMA: &str = include_str!("../../schema/messages_001.sql");
    // items.id=406: reviewed_at_risk_rating is added in messages_003.sql --
    // this in-memory test DB must apply it too (002's chat_id column is
    // untouched by this module's own queries, but harmless to include).
    const MESSAGES_SCHEMA_V2: &str = include_str!("../../schema/messages_002.sql");
    const MESSAGES_SCHEMA_V3: &str = include_str!("../../schema/messages_003.sql");
    // items.id=587: is_error -- row_to_message_record now reads this column
    // unconditionally, so this in-memory test DB must apply it too.
    const MESSAGES_SCHEMA_V4: &str = include_str!("../../schema/messages_004.sql");
    // items.id=501 slice 3: drops gate3_review_status/reviewed_at_risk_rating,
    // which row_to_message_record no longer reads.
    const MESSAGES_SCHEMA_V5: &str = include_str!("../../schema/messages_005.sql");

    async fn test_db() -> SqliteConnection {
        let mut conn = SqliteConnectOptions::new()
            .filename(":memory:")
            .connect()
            .await
            .expect("in-memory connection failed");
        for stmt in parse_statements(MESSAGES_SCHEMA)
            .into_iter()
            .chain(parse_statements(MESSAGES_SCHEMA_V2))
            .chain(parse_statements(MESSAGES_SCHEMA_V3))
            .chain(parse_statements(MESSAGES_SCHEMA_V4))
            .chain(parse_statements(MESSAGES_SCHEMA_V5))
        {
            sqlx::query(&stmt)
                .execute(&mut conn)
                .await
                .unwrap_or_else(|e| panic!("schema statement failed: {e}\n{stmt}"));
        }
        conn
    }

    /// Insert a message row directly, bypassing save_message (which requires
    /// a real messages.db path). Returns the message id.
    async fn seed_message(
        conn: &mut SqliteConnection,
        context_key: &str,
        sender: &str,
        content: &str,
        created_at: &str,
    ) -> String {
        let id = uuid::Uuid::new_v4().to_string();
        sqlx::query(
            "INSERT INTO messages (id, context_key, sender, content, created_at)
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(context_key)
        .bind(sender)
        .bind(content)
        .bind(created_at)
        .execute(&mut *conn)
        .await
        .expect("messages insert failed");
        id
    }

    #[tokio::test]
    async fn schema_rejects_invalid_sender() {
        let mut conn = test_db().await;

        let result = sqlx::query(
            "INSERT INTO messages (id, context_key, sender, content, created_at)
             VALUES ('id-1', 'ctx-1', 'not_a_real_sender', 'hi', 'now')",
        )
        .execute(&mut conn)
        .await;

        assert!(
            result.is_err(),
            "CHECK constraint must reject an unrecognized sender value"
        );
    }

    #[tokio::test]
    async fn row_to_message_record_maps_every_column() {
        let mut conn = test_db().await;
        let id = seed_message(&mut conn, "ctx-1", "user", "hello", "2026-08-09T00:00:00Z").await;

        let row = sqlx::query(
            "SELECT id, context_key, sender, content, focus_run_id, created_at, is_error
             FROM messages WHERE id = ?",
        )
        .bind(&id)
        .fetch_one(&mut conn)
        .await
        .expect("query failed");

        let record = row_to_message_record(&row).expect("row mapping failed");
        assert_eq!(record.id, id);
        assert_eq!(record.context_key, "ctx-1");
        assert_eq!(record.sender, "user");
        assert_eq!(record.content, "hello");
        assert_eq!(record.focus_run_id, None);
        assert_eq!(record.created_at, "2026-08-09T00:00:00Z");
        assert!(
            !record.is_error,
            "a freshly seeded row must default is_error to false"
        );
    }

    // -----------------------------------------------------------------
    // Real-encrypted-path tests -- save_message/list_messages
    // themselves, via migrate_messages_db,
    // mirroring commands/library.rs's TestEnv pattern. Everything above
    // this point tests the schema/query shape directly against an
    // in-memory connection; these exercise the actual public functions
    // Phase 2's IPC commands call.
    // -----------------------------------------------------------------

    use crate::test_support::ENV_MUTEX;

    const USER_ID: &str = "user-msg-test";
    const PERSONA_ID: &str = "persona-msg-test";
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
    async fn save_message_then_list_messages_round_trips_through_the_real_encrypted_path() {
        let _env = setup().await;

        save_message(
            USER_ID,
            PERSONA_ID,
            KEY_HEX,
            "persona-hub-persona-1",
            "user",
            "hello there",
            None,
        )
        .await
        .expect("save_message (user turn) must succeed");

        save_message(
            USER_ID,
            PERSONA_ID,
            KEY_HEX,
            "persona-hub-persona-1",
            "assistant",
            "hi, how can I help?",
            Some("run-1"),
        )
        .await
        .expect("save_message (assistant turn) must succeed");

        let transcript = list_messages(USER_ID, PERSONA_ID, KEY_HEX, "persona-hub-persona-1")
            .await
            .expect("list_messages must succeed");

        assert_eq!(transcript.len(), 2);
        assert_eq!(transcript[0].sender, "user");
        assert_eq!(transcript[0].content, "hello there");
        assert_eq!(transcript[1].sender, "assistant");
        assert_eq!(transcript[1].focus_run_id.as_deref(), Some("run-1"));
    }

    #[tokio::test]
    async fn list_messages_does_not_bleed_across_context_keys() {
        let _env = setup().await;

        save_message(
            USER_ID,
            PERSONA_ID,
            KEY_HEX,
            "persona-hub-persona-1",
            "user",
            "persona hub message",
            None,
        )
        .await
        .expect("save_message must succeed");

        save_message(
            USER_ID,
            PERSONA_ID,
            KEY_HEX,
            "tier3-access-persona-1",
            "user",
            "tier3 message",
            None,
        )
        .await
        .expect("save_message must succeed");

        let persona_hub_transcript =
            list_messages(USER_ID, PERSONA_ID, KEY_HEX, "persona-hub-persona-1")
                .await
                .expect("list_messages must succeed");
        let tier3_transcript =
            list_messages(USER_ID, PERSONA_ID, KEY_HEX, "tier3-access-persona-1")
                .await
                .expect("list_messages must succeed");

        assert_eq!(persona_hub_transcript.len(), 1);
        assert_eq!(persona_hub_transcript[0].content, "persona hub message");
        assert_eq!(tier3_transcript.len(), 1);
        assert_eq!(tier3_transcript[0].content, "tier3 message");
    }

    #[tokio::test]
    async fn get_message_returns_the_saved_row() {
        let _env = setup().await;

        let record = save_message(
            USER_ID,
            PERSONA_ID,
            KEY_HEX,
            "tier3-access-persona-1",
            "assistant",
            "drafted starter text",
            Some("run-1"),
        )
        .await
        .expect("save_message must succeed");

        let fetched = get_message(USER_ID, PERSONA_ID, KEY_HEX, &record.id)
            .await
            .expect("get_message must succeed")
            .expect("message must exist");

        assert_eq!(fetched.id, record.id);
        assert_eq!(fetched.content, "drafted starter text");
        assert_eq!(fetched.focus_run_id.as_deref(), Some("run-1"));
    }

    #[tokio::test]
    async fn get_message_returns_none_for_unknown_id() {
        let _env = setup().await;

        let fetched = get_message(USER_ID, PERSONA_ID, KEY_HEX, "no-such-id")
            .await
            .expect("get_message must succeed");

        assert!(fetched.is_none());
    }

    #[tokio::test]
    async fn save_message_rejects_invalid_sender() {
        let _env = setup().await;

        let result = save_message(
            USER_ID,
            PERSONA_ID,
            KEY_HEX,
            "ctx-1",
            "not_a_real_sender",
            "hi",
            None,
        )
        .await;

        assert!(matches!(result, Err(MessageStoreError::Validation(_))));
    }

    #[tokio::test]
    async fn open_messages_db_heals_a_pre_existing_v1_only_database() {
        // Regression test for the bug found live 2026-09-01 (items.id=384
        // slice 7): open_messages_db used to call migrate_messages_db ONLY
        // when the file didn't exist yet ("if !db_path.exists()"), so a
        // messages.db created before messages_002.sql existed stayed
        // stuck on schema v1 forever, no matter how many times the app
        // reopened it -- confirmed live against a real pre-existing dev
        // account (list_chats failing with "no such table: chats"). Hand-
        // builds that stale v1-only shape directly against a real
        // encrypted file (SCHEMA_FILES is a compile-time static that
        // always includes v2 now, so migrate_messages_db itself can't
        // produce a deliberately-stale fixture -- same constraint
        // migrations.rs's own
        // run_pending_heals_content_drift_in_stale_v1_database documents).
        let lock = ENV_MUTEX.lock().await;
        let saved_root = std::env::var("QR_DATA_ROOT").ok();
        let tempdir = tempfile::tempdir().expect("failed to create tempdir");
        std::env::set_var("QR_DATA_ROOT", tempdir.path());

        let user_id = "user-stale-v1-test";
        let persona_id = "persona-stale-v1-test";
        let db_path = get_messages_db_path(user_id, persona_id);
        std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();

        {
            let mut conn = crate::providers::utils::connect_options_encrypted(&db_path, KEY_HEX)
                .create_if_missing(true)
                .connect()
                .await
                .expect("stale fixture connect failed");
            for stmt in parse_statements(MESSAGES_SCHEMA) {
                sqlx::query(&stmt)
                    .execute(&mut conn)
                    .await
                    .unwrap_or_else(|e| {
                        panic!("stale fixture schema statement failed: {e}\n{stmt}")
                    });
            }
        }
        assert!(db_path.exists(), "stale v1-only fixture file must exist");

        // The real function under test -- must heal the stale file, not
        // just successfully connect to it as-is.
        open_messages_db(user_id, persona_id, KEY_HEX)
            .await
            .expect("open_messages_db must heal the stale v1 database, not error");

        let mut verify_conn = open_messages_db(user_id, persona_id, KEY_HEX)
            .await
            .expect("re-open must succeed");
        let exists: Option<(String,)> =
            sqlx::query_as("SELECT name FROM sqlite_master WHERE type='table' AND name='chats'")
                .fetch_optional(&mut verify_conn)
                .await
                .unwrap();
        assert!(
            exists.is_some(),
            "chats table must exist after opening a pre-existing v1-only messages.db -- \
             open_messages_db must run pending migrations on every open, not only when \
             the file doesn't exist yet"
        );

        match saved_root {
            Some(v) => std::env::set_var("QR_DATA_ROOT", v),
            None => std::env::remove_var("QR_DATA_ROOT"),
        }
        drop(lock);
    }

    /// Column names of the messages table, via pragma_table_info.
    async fn messages_columns(conn: &mut SqliteConnection) -> Vec<String> {
        sqlx::query_scalar::<_, String>("SELECT name FROM pragma_table_info('messages')")
            .fetch_all(conn)
            .await
            .expect("pragma_table_info must succeed")
    }

    async fn messages_applied_versions(conn: &mut SqliteConnection) -> Vec<i64> {
        sqlx::query_scalar::<_, i64>("SELECT version FROM schema_version ORDER BY version")
            .fetch_all(conn)
            .await
            .expect("schema_version must be readable")
    }

    /// items.id=501 slice 3: messages_005.sql drops gate3_review_status and
    /// reviewed_at_risk_rating (and their column-level CHECKs). Builds a
    /// real v4-format SQLCipher file -- one row per old status, with risk
    /// ratings, chat_id and is_error populated -- then lets open_messages_db
    /// run the real migration and checks every surviving column, the index
    /// and a fresh write. Same stale-fixture technique as
    /// open_messages_db_heals_a_pre_existing_v1_only_database, and it runs on
    /// the real SQLCipher linkage, so it also proves DROP COLUMN works there.
    #[tokio::test]
    async fn messages_005_drops_review_columns_and_preserves_every_old_row() {
        let lock = ENV_MUTEX.lock().await;
        let saved_root = std::env::var("QR_DATA_ROOT").ok();
        let tempdir = tempfile::tempdir().expect("failed to create tempdir");
        std::env::set_var("QR_DATA_ROOT", tempdir.path());

        let user_id = "user-005-test";
        let persona_id = "persona-005-test";
        let db_path = get_messages_db_path(user_id, persona_id);
        std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();

        // (id, status, risk) -- every value the old CHECK allowed, plus NULL.
        let old_rows: [(&str, Option<&str>, Option<i64>); 5] = [
            ("m-null", None, None),
            ("m-drafted", Some("drafted"), None),
            ("m-pending", Some("pending-review"), Some(2)),
            ("m-approved", Some("approved"), Some(3)),
            ("m-withheld", Some("withheld"), Some(1)),
        ];
        {
            let mut conn = crate::providers::utils::connect_options_encrypted(&db_path, KEY_HEX)
                .create_if_missing(true)
                .connect()
                .await
                .expect("v4 fixture connect failed");
            for schema in [
                MESSAGES_SCHEMA,
                MESSAGES_SCHEMA_V2,
                MESSAGES_SCHEMA_V3,
                MESSAGES_SCHEMA_V4,
            ] {
                for stmt in parse_statements(schema) {
                    sqlx::query(&stmt)
                        .execute(&mut conn)
                        .await
                        .unwrap_or_else(|e| panic!("v4 fixture statement failed: {e}\n{stmt}"));
                }
            }
            assert_eq!(messages_applied_versions(&mut conn).await, vec![1, 2, 3, 4]);
            assert!(messages_columns(&mut conn)
                .await
                .contains(&"gate3_review_status".to_owned()));
            for (i, (id, status, risk)) in old_rows.iter().enumerate() {
                sqlx::query(
                    "INSERT INTO messages
                     (id, context_key, sender, content, focus_run_id, gate3_review_status,
                      created_at, chat_id, reviewed_at_risk_rating, is_error)
                     VALUES (?, 'chat-1', 'assistant', ?, ?, ?, ?, 'chat-1', ?, ?)",
                )
                .bind(id)
                .bind(format!("content {id}"))
                .bind(format!("run-{id}"))
                .bind(status)
                .bind(format!("2026-10-0{}T00:00:00Z", i + 1))
                .bind(risk)
                .bind(if *id == "m-pending" { 1_i64 } else { 0_i64 })
                .execute(&mut conn)
                .await
                .expect("old-format row insert must succeed");
            }
        }

        // The real function under test: opens, finds v4, applies v5.
        let mut conn = open_messages_db(user_id, persona_id, KEY_HEX)
            .await
            .expect("open_messages_db must migrate a v4 database to v5");

        let cols = messages_columns(&mut conn).await;
        assert!(!cols.contains(&"gate3_review_status".to_owned()));
        assert!(!cols.contains(&"reviewed_at_risk_rating".to_owned()));
        for kept in [
            "id",
            "context_key",
            "sender",
            "content",
            "focus_run_id",
            "created_at",
            "chat_id",
            "is_error",
        ] {
            assert!(
                cols.contains(&kept.to_owned()),
                "column {kept} must survive"
            );
        }
        assert_eq!(
            messages_applied_versions(&mut conn).await,
            vec![1, 2, 3, 4, 5]
        );

        // Every old row loads through the real read path, other columns intact.
        let transcript = list_messages(user_id, persona_id, KEY_HEX, "chat-1")
            .await
            .expect("list_messages must read migrated rows");
        assert_eq!(transcript.len(), old_rows.len());
        for (i, (id, _, _)) in old_rows.iter().enumerate() {
            let m = &transcript[i];
            assert_eq!(&m.id, id);
            assert_eq!(m.content, format!("content {id}"));
            assert_eq!(m.sender, "assistant");
            assert_eq!(
                m.focus_run_id.as_deref(),
                Some(format!("run-{id}").as_str())
            );
            assert_eq!(m.created_at, format!("2026-10-0{}T00:00:00Z", i + 1));
            assert_eq!(m.is_error, *id == "m-pending");
        }
        let chat_ids: Vec<Option<String>> =
            sqlx::query_scalar("SELECT chat_id FROM messages ORDER BY created_at")
                .fetch_all(&mut conn)
                .await
                .unwrap();
        assert!(chat_ids.iter().all(|c| c.as_deref() == Some("chat-1")));

        let idx: Option<String> = sqlx::query_scalar(
            "SELECT name FROM sqlite_master WHERE type='index' AND name='idx_messages_context_key_created'",
        )
        .fetch_optional(&mut conn)
        .await
        .unwrap();
        assert!(idx.is_some(), "the context_key index must survive the drop");

        save_message(
            user_id,
            persona_id,
            KEY_HEX,
            "chat-1",
            "user",
            "written after the migration",
            None,
        )
        .await
        .expect("save_message must work on the migrated schema");

        match saved_root {
            Some(v) => std::env::set_var("QR_DATA_ROOT", v),
            None => std::env::remove_var("QR_DATA_ROOT"),
        }
        drop(lock);
    }

    /// A DB built fresh through every migration, closed, and reopened: the
    /// columns stay absent and startup is stable. messages_001.sql (v1)
    /// re-runs on EVERY open, and its CREATE TABLE still names the dropped
    /// columns -- CREATE TABLE IF NOT EXISTS must not resurrect them, and
    /// v5 must not re-apply (a second DROP COLUMN would error).
    #[tokio::test]
    async fn fresh_messages_db_is_stable_across_close_and_reopen() {
        let _env = setup().await;

        let mut conn = open_messages_db(USER_ID, PERSONA_ID, KEY_HEX)
            .await
            .expect("first open must succeed");
        let cols = messages_columns(&mut conn).await;
        assert!(!cols.contains(&"gate3_review_status".to_owned()));
        assert!(!cols.contains(&"reviewed_at_risk_rating".to_owned()));
        assert_eq!(
            messages_applied_versions(&mut conn).await,
            vec![1, 2, 3, 4, 5]
        );
        conn.close().await.expect("close must succeed");

        for _ in 0..2 {
            let mut conn = open_messages_db(USER_ID, PERSONA_ID, KEY_HEX)
                .await
                .expect("reopen must succeed");
            assert_eq!(messages_columns(&mut conn).await, cols);
            assert_eq!(
                messages_applied_versions(&mut conn).await,
                vec![1, 2, 3, 4, 5]
            );
            conn.close().await.expect("close must succeed");
        }

        save_message(
            USER_ID,
            PERSONA_ID,
            KEY_HEX,
            "ctx-1",
            "user",
            "after reopen",
            None,
        )
        .await
        .expect("save_message must work after reopen");
    }

    #[tokio::test]
    async fn open_messages_db_rejects_empty_key_hex_with_a_typed_validation_error() {
        let _env = setup().await;

        let result = list_messages(USER_ID, PERSONA_ID, "", "ctx-1").await;

        match result.unwrap_err() {
            MessageStoreError::Validation(msg) => {
                assert!(msg.contains("key_hex"), "unexpected message: {msg}")
            }
            other => panic!("expected Validation variant, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn list_ordering_and_context_key_scoping_is_correct() {
        // Exercises the ORDER BY created_at ASC + WHERE context_key = ?
        // shape directly against the schema, without requiring a real
        // messages.db path (list_messages() itself needs one via
        // open_messages_db, so this covers the query logic list_messages
        // wraps).
        let mut conn = test_db().await;
        seed_message(&mut conn, "ctx-a", "user", "first", "2026-08-09T00:00:01Z").await;
        seed_message(
            &mut conn,
            "ctx-a",
            "assistant",
            "second",
            "2026-08-09T00:00:02Z",
        )
        .await;
        seed_message(
            &mut conn,
            "ctx-b",
            "user",
            "other context",
            "2026-08-09T00:00:03Z",
        )
        .await;

        let rows = sqlx::query(
            "SELECT content FROM messages WHERE context_key = ? ORDER BY created_at ASC",
        )
        .bind("ctx-a")
        .fetch_all(&mut conn)
        .await
        .expect("query failed");

        assert_eq!(rows.len(), 2, "must only return ctx-a's messages");
        let first: String = rows[0].try_get("content").unwrap();
        let second: String = rows[1].try_get("content").unwrap();
        assert_eq!(first, "first");
        assert_eq!(second, "second");
    }
}
