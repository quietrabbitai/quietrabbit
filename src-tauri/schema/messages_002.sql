-- messages_002.sql
--
-- decisions.id=739/740 (items.id=384 slice 6): persona-scoped chat
-- history. `messages` (messages_001.sql) has always been a flat table
-- keyed by context_key -- no session/chat boundary at all (confirmed,
-- items.id=318/381). This adds that boundary: a `chats` table (one row
-- per persona-scoped conversation) plus a nullable `chat_id` column on
-- `messages` linking new rows to it.
--
-- `chat_id` is nullable and NOT backfilled onto existing rows: the
-- pre-existing context_key-based rows ("persona-hub-*"/"tier3-access-*"
-- strings) keep working exactly as before, unmigrated -- context_key
-- remains their join key. chat_id is authoritative only for chats
-- created going forward (commands::chats::create_chat), whose
-- context_key is "chat-{uuid}".
--
-- archived_at (not a hard delete): decisions.id=739 -- explicit delete
-- is a distinct future action, never implicit or bundled with starting
-- a new chat.
--
-- SCHEMA AUTHORING RULE (migrations.rs): no semicolons inside string
-- literals -- parse_statements() is not a general-purpose SQL parser.

CREATE TABLE IF NOT EXISTS chats (
    id              TEXT PRIMARY KEY,
    persona_id      TEXT NOT NULL,
    context_key     TEXT NOT NULL UNIQUE,
    title           TEXT,
    archived_at     TEXT,
    created_at      TEXT NOT NULL,
    last_message_at TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_chats_persona_active
    ON chats (persona_id, archived_at, last_message_at);

ALTER TABLE messages ADD COLUMN chat_id TEXT;

INSERT OR IGNORE INTO schema_version (version, applied_at, description)
VALUES (2, datetime('now'),
    'decisions.id=739/740 (items.id=384): chats table + messages.chat_id for persona-scoped chat history');
