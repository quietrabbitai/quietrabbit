-- messages_006.sql
--
-- items.id=715 (decisions.id=883): a streamed reply from an external provider
-- is shown as it arrives, and Gate 2 then scans the COMPLETE text. A hit never
-- holds, discards or edits the reply; it is recorded on the reply itself so
-- the transcript can show a plain, non-blocking notice and so a later forward
-- of this content can see the mark. The flag lives on the message row because
-- the reply IS the message row: list_messages already returns it, and it
-- travels with the content in the per-Persona encrypted messages.db.
--
-- NOT NULL DEFAULT 0: every existing row predates this and was never flagged.
--
-- SCHEMA AUTHORING RULE (migrations.rs): no semicolons inside string
-- literals -- parse_statements() is not a general-purpose SQL parser.

ALTER TABLE messages ADD COLUMN personal_flagged INTEGER NOT NULL DEFAULT 0
    CHECK (personal_flagged IN (0, 1));

INSERT OR IGNORE INTO schema_version (version, applied_at, description)
VALUES (6, datetime('now'),
    'items.id=715 (decisions.id=883): messages.personal_flagged -- Gate 2 flagged a withheld personal value in this reply (the reply is kept and shown)');
