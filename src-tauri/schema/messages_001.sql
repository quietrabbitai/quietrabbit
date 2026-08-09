-- persistence/schema/messages_001.sql
-- Per-user, per-persona chat/transcript database: messages.db
-- Encrypted with SQLCipher using user master key.
-- Path: /users/{user_id}/personas/{persona_id}/messages.db
--
-- Backs ChatPane (frontend/src/chat/ChatPane.tsx), the real component behind
-- MiddleZone's chatPane prop for both Persona hub chat and Tier3AccessPane's
-- starter-drafting pane (items.id=245-ish -- see that item's plan for the
-- full design). One shared table for both purposes: gate3_review_status is
-- NULL for persona-hub messages, populated only for Tier3-context messages.
--
-- No soft-delete/tombstone machinery here (contrast outputs_001.sql) --
-- messages have no delete affordance in this item's scope.

CREATE TABLE IF NOT EXISTS schema_version (
    version         INTEGER PRIMARY KEY,
    applied_at      TEXT NOT NULL,
    description     TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS migration_lock (
    id          INTEGER PRIMARY KEY CHECK (id = 1),
    locked_at   TEXT,
    locked_by   TEXT
);
INSERT OR IGNORE INTO migration_lock (id) VALUES (1);

-- context_key mirrors MiddleZone's contextKey prop verbatim (the caller-owned
-- transcript identity MiddleZone's own doc comment says it does not own) --
-- e.g. "persona-hub-{personaId}" or "tier3-access-{personaId}".
--
-- gate3_review_status: drafted -> pending-review -> approved | withheld.
-- NULL for persona-hub messages (no Tier 3 handoff involved). Populated only
-- on the assistant-turn row for Tier3-context messages, starting at
-- 'drafted'. gate3 IPC wiring itself (items.id=233's remaining stub) is not
-- built by this item -- this column is written by this item, transitioned by
-- that one.
CREATE TABLE IF NOT EXISTS messages (
    id                      TEXT PRIMARY KEY,
    context_key             TEXT NOT NULL,
    sender                  TEXT NOT NULL CHECK (sender IN ('user', 'assistant')),
    content                 TEXT NOT NULL,
    focus_run_id            TEXT,
    gate3_review_status     TEXT
                                CHECK (gate3_review_status IS NULL OR gate3_review_status IN (
                                    'drafted', 'pending-review', 'approved', 'withheld'
                                )),
    created_at              TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_messages_context_key_created
    ON messages (context_key, created_at);

INSERT OR IGNORE INTO schema_version (version, applied_at, description)
    VALUES (1, datetime('now'), 'messages: initial schema');
