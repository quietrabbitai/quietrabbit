-- personal_011.sql
--
-- items.id=891 stage 1 (decisions.id=940 ruling 2, decisions.id=509): entity
-- OWNERSHIP by a Focus. Number checked against the live schema directory on the
-- build date: personal_010 was the latest. Numbers before the items.id=633
-- cutover are provisional, since the cutover folds every prefix into one
-- baseline.
--
-- entities.owner_focus: the Focus (focuses.focus_id, a slug) that created the
-- entity and is the only Focus allowed to read or write it. NULL means
-- UNOWNED, which is every entity that exists today (imports, synced shares,
-- Persona-wide records) and stays the default: a row never written by a Focus
-- op is never owned.
--
-- WHY A COLUMN. entities and entity_facts are Persona-wide and carry no Focus.
-- Everything that copies or loads them (persona share, persona sync, view-only
-- sync, the PersonalTrack load of every Focus run) reads the whole Persona. An
-- owned entity (a Tech Support device) must never be copied to another Persona
-- and its facts must never reach another Focus, so those readers filter on
-- owner_focus IS NULL (entity_store::OwnerFilter, with a source scan that pins
-- an explicit owner filter at every call site). The only code that sets the
-- column is the Focus-owned create path (entity_store), and the only code that
-- hard-deletes an owned entity is entity_purge (items.id=891 stage 4).
--
-- Nothing indexes the entity's Focus beyond this one index: the Focus-scoped
-- list reads (device library) and the purge reconcile scan by (owner_focus,
-- status).
--
-- ALTER TABLE ... ADD COLUMN precedent: personal_003.sql (no rebuild needed).
-- The new column is nullable with no default, so no existing row changes.
--
-- SCHEMA AUTHORING RULE (migrations.rs): no semicolons inside string literals or
-- comments -- parse_statements() is not a general-purpose SQL parser.

ALTER TABLE entities ADD COLUMN owner_focus TEXT;

CREATE INDEX IF NOT EXISTS idx_entities_owner_focus
    ON entities (owner_focus, status);

INSERT OR IGNORE INTO schema_version (version, applied_at, description)
VALUES (11, datetime('now'),
    'items.id=891 (decisions.id=940, 509): entities.owner_focus, the Focus that owns an entity (NULL = unowned), so persona share, sync and every other Focus can exclude it');
