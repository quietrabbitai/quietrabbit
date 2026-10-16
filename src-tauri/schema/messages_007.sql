-- messages_007.sql
--
-- items.id=716 (decisions.id=874, 875): API Chat. An API conversation is its
-- own chat, bound at creation to one provider and model, so external use is
-- always apparent and QR chat stays self-contained.
--
-- chat_bindings: one row per bound conversation, keyed by context_key. NOT
-- columns on `chats`: a `chats` row is created lazily, only once a real reply
-- is saved (items.id=587, decisions.id=844), but the binding has to exist
-- before the first run. A context_key with no row here is a QR chat (every
-- chat that existed before this migration).
--
-- kind is TEXT with no CHECK on purpose: the set is meant to grow (a local
-- large-model kind later) and SQLite cannot change a CHECK without rebuilding
-- the table. The Rust enum ChatKind validates it and refuses an unknown value.
--
-- chat_binding_targets: the provider and model of each slot. Only slot 0 is
-- used today. A later fan-out adds slots 1 and 2 with no schema change. Bound
-- by provider_id and model_id strings, never by a name in code (decisions.id=872).
-- Rows are never updated: a binding is immutable (the store has no update path).
--
-- messages.provider_id and messages.model_id record which provider and model
-- produced an assistant reply (NULL for a user turn, a QR reply, or any row
-- from before this migration). The history builder uses them to keep a reply
-- Gate 2 flagged from being sent to a different provider (decisions.id=883).
--
-- messages.reply_status: complete, truncated, cancelled or cut. A reply that
-- was shown and then cancelled or cut keeps its text and says so (decisions.id=883:
-- displayed text is never retracted). Validated in Rust, no CHECK, for the same
-- reason as kind.
--
-- messages.outbound_content: for a user turn in an API chat, the text that was
-- actually sent to the provider after the Privacy Guardian review applied the
-- user's decisions. History is rebuilt from this column, so a span the user
-- already decided on is never reviewed or sent again. NULL everywhere else.
--
-- SCHEMA AUTHORING RULE (migrations.rs): no semicolons inside string
-- literals -- parse_statements() is not a general-purpose SQL parser.

CREATE TABLE IF NOT EXISTS chat_bindings (
    context_key TEXT PRIMARY KEY,
    kind        TEXT NOT NULL,
    created_at  TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS chat_binding_targets (
    context_key TEXT NOT NULL REFERENCES chat_bindings(context_key),
    slot        INTEGER NOT NULL DEFAULT 0,
    provider_id TEXT NOT NULL,
    model_id    TEXT NOT NULL,
    created_at  TEXT NOT NULL,
    PRIMARY KEY (context_key, slot)
);

ALTER TABLE messages ADD COLUMN provider_id TEXT;
ALTER TABLE messages ADD COLUMN model_id TEXT;
ALTER TABLE messages ADD COLUMN reply_status TEXT NOT NULL DEFAULT 'complete';
ALTER TABLE messages ADD COLUMN outbound_content TEXT;

INSERT OR IGNORE INTO schema_version (version, applied_at, description)
VALUES (7, datetime('now'),
    'items.id=716: chat_bindings and chat_binding_targets for API Chat, plus messages.provider_id, model_id, reply_status and outbound_content');
