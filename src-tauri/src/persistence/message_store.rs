// src-tauri/src/persistence/message_store.rs
//
// Chat/transcript message persistence for messages.db — per-user, per-persona,
// SQLCipher encrypted. Path: /users/{user_id}/personas/{persona_id}/messages.db
//
// Backs commands/messages.rs (send_message/list_messages), which in turn
// backs ChatPane.tsx -- the real component behind MiddleZone's chatPane prop
// for both Persona hub chat and Tier3AccessPane's starter-drafting pane.
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
const VALID_GATE3_REVIEW_STATUS: &[&str] = &["drafted", "pending-review", "approved", "withheld"];

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
    pub gate3_review_status: Option<String>,
    pub created_at: String,
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
        gate3_review_status: r.try_get("gate3_review_status")?,
        created_at: r.try_get("created_at")?,
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
    gate3_review_status: Option<&str>,
) -> Result<MessageRecord, MessageStoreError> {
    if !VALID_SENDER.contains(&sender) {
        return Err(MessageStoreError::Validation(format!(
            "Invalid sender '{}'. Must be one of: {}",
            sender,
            VALID_SENDER.join(", ")
        )));
    }
    if let Some(status) = gate3_review_status {
        if !VALID_GATE3_REVIEW_STATUS.contains(&status) {
            return Err(MessageStoreError::Validation(format!(
                "Invalid gate3_review_status '{}'. Must be one of: {}",
                status,
                VALID_GATE3_REVIEW_STATUS.join(", ")
            )));
        }
    }

    let id = uuid::Uuid::new_v4().to_string();
    let timestamp = crate::providers::utils::now();
    let mut conn = open_messages_db(user_id, persona_id, key_hex).await?;

    sqlx::query(
        "INSERT INTO messages
         (id, context_key, sender, content, focus_run_id, gate3_review_status, created_at)
         VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(context_key)
    .bind(sender)
    .bind(content)
    .bind(focus_run_id)
    .bind(gate3_review_status)
    .bind(&timestamp)
    .execute(&mut conn)
    .await?;

    Ok(MessageRecord {
        id,
        context_key: context_key.to_owned(),
        sender: sender.to_owned(),
        content: content.to_owned(),
        focus_run_id: focus_run_id.map(|s| s.to_owned()),
        gate3_review_status: gate3_review_status.map(|s| s.to_owned()),
        created_at: timestamp,
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
        "SELECT id, context_key, sender, content, focus_run_id, gate3_review_status, created_at
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

/// Fetch a single message by id — the read a gate3-review command needs
/// before mutating gate3_review_status. Returns None if not found (an
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
        "SELECT id, context_key, sender, content, focus_run_id, gate3_review_status, created_at
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

// ---------------------------------------------------------------------------
// Update
// ---------------------------------------------------------------------------

/// Backfill a placeholder assistant message's content once its focus run
/// finishes generating (commands::messages::send_message spawns a background
/// task that awaits execute_full() then calls this). Narrow single-column
/// update, same shape as update_gate3_review_status below.
pub async fn update_message_content(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
    message_id: &str,
    content: &str,
) -> Result<(), MessageStoreError> {
    let mut conn = open_messages_db(user_id, persona_id, key_hex).await?;

    sqlx::query("UPDATE messages SET content = ? WHERE id = ?")
        .bind(content)
        .bind(message_id)
        .execute(&mut conn)
        .await?;

    Ok(())
}

/// Update a single message's gate3_review_status. Narrow single-column
/// update — for the future gate3-wiring item (items.id=233's remaining
/// stub) to call once it exists; not called from anywhere in this item's own
/// code. Included now because the schema/store boundary is the right place
/// for it.
pub async fn update_gate3_review_status(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
    message_id: &str,
    status: &str,
) -> Result<(), MessageStoreError> {
    if !VALID_GATE3_REVIEW_STATUS.contains(&status) {
        return Err(MessageStoreError::Validation(format!(
            "Invalid gate3_review_status '{}'. Must be one of: {}",
            status,
            VALID_GATE3_REVIEW_STATUS.join(", ")
        )));
    }

    let mut conn = open_messages_db(user_id, persona_id, key_hex).await?;

    sqlx::query("UPDATE messages SET gate3_review_status = ? WHERE id = ?")
        .bind(status)
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

    const MESSAGES_SCHEMA: &str = include_str!("../../schema/messages_001.sql");

    async fn test_db() -> SqliteConnection {
        let mut conn = SqliteConnectOptions::new()
            .filename(":memory:")
            .connect()
            .await
            .expect("in-memory connection failed");
        for stmt in parse_statements(MESSAGES_SCHEMA) {
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
    async fn schema_accepts_a_full_row_including_gate3_review_status() {
        let mut conn = test_db().await;
        let id = uuid::Uuid::new_v4().to_string();

        sqlx::query(
            "INSERT INTO messages
             (id, context_key, sender, content, focus_run_id, gate3_review_status, created_at)
             VALUES (?, 'tier3-access-persona-1', 'assistant', 'draft text',
                     'run-1', 'drafted', '2026-08-09T00:00:00Z')",
        )
        .bind(&id)
        .execute(&mut conn)
        .await
        .expect("full row insert must succeed");

        let row = sqlx::query("SELECT gate3_review_status FROM messages WHERE id = ?")
            .bind(&id)
            .fetch_one(&mut conn)
            .await
            .expect("query failed");
        let status: Option<String> = row.try_get("gate3_review_status").unwrap();
        assert_eq!(status.as_deref(), Some("drafted"));
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
    async fn schema_rejects_invalid_gate3_review_status() {
        let mut conn = test_db().await;

        let result = sqlx::query(
            "INSERT INTO messages (id, context_key, sender, content, gate3_review_status, created_at)
             VALUES ('id-1', 'ctx-1', 'assistant', 'hi', 'not_a_real_status', 'now')",
        )
        .execute(&mut conn)
        .await;

        assert!(
            result.is_err(),
            "CHECK constraint must reject an unrecognized gate3_review_status value"
        );
    }

    #[tokio::test]
    async fn schema_accepts_null_gate3_review_status_for_persona_hub_messages() {
        let mut conn = test_db().await;

        let result = sqlx::query(
            "INSERT INTO messages (id, context_key, sender, content, created_at)
             VALUES ('id-1', 'persona-hub-persona-1', 'user', 'hi', 'now')",
        )
        .execute(&mut conn)
        .await;

        assert!(
            result.is_ok(),
            "gate3_review_status must be nullable for persona-hub messages"
        );
    }

    #[tokio::test]
    async fn row_to_message_record_maps_every_column() {
        let mut conn = test_db().await;
        let id = seed_message(&mut conn, "ctx-1", "user", "hello", "2026-08-09T00:00:00Z").await;

        let row = sqlx::query(
            "SELECT id, context_key, sender, content, focus_run_id, gate3_review_status, created_at
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
        assert_eq!(record.gate3_review_status, None);
        assert_eq!(record.created_at, "2026-08-09T00:00:00Z");
    }

    // -----------------------------------------------------------------
    // Real-encrypted-path tests -- save_message/list_messages/
    // update_gate3_review_status themselves, via migrate_messages_db,
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
        _lock: std::sync::MutexGuard<'static, ()>,
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
        let lock = ENV_MUTEX.lock().unwrap();
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
            None,
        )
        .await
        .expect("save_message (assistant turn) must succeed");

        let transcript = list_messages(USER_ID, PERSONA_ID, KEY_HEX, "persona-hub-persona-1")
            .await
            .expect("list_messages must succeed");

        assert_eq!(transcript.len(), 2);
        assert_eq!(transcript[0].sender, "user");
        assert_eq!(transcript[0].content, "hello there");
        assert_eq!(transcript[0].gate3_review_status, None);
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
            Some("drafted"),
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
        assert_eq!(fetched.gate3_review_status.as_deref(), Some("drafted"));
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
    async fn update_gate3_review_status_transitions_an_existing_message() {
        let _env = setup().await;

        let record = save_message(
            USER_ID,
            PERSONA_ID,
            KEY_HEX,
            "tier3-access-persona-1",
            "assistant",
            "drafted starter text",
            Some("run-1"),
            Some("drafted"),
        )
        .await
        .expect("save_message must succeed");

        update_gate3_review_status(USER_ID, PERSONA_ID, KEY_HEX, &record.id, "approved")
            .await
            .expect("update_gate3_review_status must succeed");

        let transcript = list_messages(USER_ID, PERSONA_ID, KEY_HEX, "tier3-access-persona-1")
            .await
            .expect("list_messages must succeed");

        assert_eq!(transcript.len(), 1);
        assert_eq!(
            transcript[0].gate3_review_status.as_deref(),
            Some("approved")
        );
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
        let lock = ENV_MUTEX.lock().unwrap();
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
