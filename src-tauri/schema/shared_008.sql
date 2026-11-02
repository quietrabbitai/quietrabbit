-- shared_008.sql
--
-- items.id=302 added pending_persona_shares.source_persona_type here, the
-- field recipient-side materialization needed to create a Persona of the
-- right type. A Persona's type is domain information and shared.db is
-- readable without the password, so items.id=661 (decisions.id=862) moved it
-- inside the encrypted PersonaSharePayload (with the display name that
-- shared_007.sql used to hold) and this file no longer adds the column.
--
-- It is edited in place, which decisions.id=857 allows until first ship. It
-- keeps its version number, because versions must stay contiguous (the
-- on-disk migration test asserts MAX(version) equals the number applied), and
-- it records that version with its own row rather than relying on the
-- runner's fallback.

INSERT OR IGNORE INTO schema_version (version, applied_at, description)
VALUES (8, datetime('now'),
    'items.id=302: persona type for recipient-side materialization (carried inside the encrypted payload since items.id=661, so this version adds no column)');
