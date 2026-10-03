-- messages_004.sql
--
-- items.id=587: a failed/paused reply with no real output is now backfilled
-- with a plain-language error message instead of staying an empty bubble
-- (see commands/messages.rs's finalize_chat_reply). That text must never be
-- replayed back into build_conversation_prompt's User:/Assistant: history on
-- a later send -- it was never something the model actually said. is_error
-- marks exactly those rows so that function can skip them, the same way it
-- already skips a still-empty placeholder.
--
-- NOT NULL DEFAULT 0: every existing row predates this feature and was, by
-- definition, never an error-backfilled row.
--
-- SCHEMA AUTHORING RULE (migrations.rs): no semicolons inside string
-- literals -- parse_statements() is not a general-purpose SQL parser.

ALTER TABLE messages ADD COLUMN is_error INTEGER NOT NULL DEFAULT 0
    CHECK (is_error IN (0, 1));

INSERT OR IGNORE INTO schema_version (version, applied_at, description)
VALUES (4, datetime('now'),
    'items.id=587: messages.is_error -- marks an error-backfilled placeholder so build_conversation_prompt never replays it as a real assistant turn');
