-- schema/personal_007.sql
-- Cross-Persona Data Provenance (decisions.id=546, items.id=27): three
-- immutable provenance fields added to entity_facts, DB-enforced.
-- Migration version: 7

-- ---------------------------------------------------------------------------
-- personal.db is one file per Persona INSTANCE (not per Persona type) --
-- path is /users/{user_id}/personas/{persona_id}/personal.db. Every row in
-- this file, before and after this migration, is physically scoped to one
-- instance already. source_persona_id therefore does NOT need a migration-
-- time backfill: nothing currently reads it (no consuming code exists yet --
-- confirmed against src-tauri/src at time of authoring), and the file's own
-- path remains the authoritative source of truth for pre-migration rows.
-- Backfill of existing rows happens as an ordinary application-level step
-- (not inside this migration) the next time personal_store.rs opens this
-- file, using the same persona_id already used to build the file path.
--
--   source_persona_id    -- Persona INSTANCE (personas.id, shared.db) this
--                            row currently lives in.
--   cross_persona_export -- 0 for every native/forked fact. 1 only when this
--                            row is a user-approved copy sitting in a Persona
--                            other than the one it originated in.
--   origin_persona_id    -- NULL unless cross_persona_export=1; names the
--                            Persona instance this fact originated in,
--                            permanently, for the required per-session
--                            re-confirmation UI (decisions.id=546).
--
-- NOT used for same-Persona-type instance forking (items.id=20 Q3/Q8):
-- forked facts are native to their new instance (cross_persona_export=0) --
-- forking within one Persona type is not a privacy-boundary crossing.
-- ---------------------------------------------------------------------------
ALTER TABLE entity_facts ADD COLUMN source_persona_id TEXT;
ALTER TABLE entity_facts ADD COLUMN cross_persona_export INTEGER NOT NULL DEFAULT 0
    CHECK (cross_persona_export IN (0, 1));
ALTER TABLE entity_facts ADD COLUMN origin_persona_id TEXT
    CHECK (origin_persona_id IS NULL OR cross_persona_export = 1);

-- ---------------------------------------------------------------------------
-- Lineage metadata for same-type instance forking (items.id=20 Q8).
-- Nullable, non-gating -- does NOT participate in the cross-Persona
-- confirmation mechanism above. Populated only by the fork-copy write path.
-- Included now rather than retrofitted later per Q8's own framing (cheap now,
-- more expensive once entity_facts holds real user data).
-- ---------------------------------------------------------------------------
ALTER TABLE entity_facts ADD COLUMN forked_from_instance_id TEXT;
ALTER TABLE entity_facts ADD COLUMN forked_at TEXT;

-- ---------------------------------------------------------------------------
-- Immutability enforcement (decisions.id=546: "unchangeable by any Conductor
-- action, Focus declaration, or user setting"). DB-level, not a convention
-- every write path has to remember -- any UPDATE touching these three
-- columns after insert is rejected outright.
-- ---------------------------------------------------------------------------
CREATE TRIGGER IF NOT EXISTS trg_entity_facts_provenance_immutable
BEFORE UPDATE OF source_persona_id, cross_persona_export, origin_persona_id
ON entity_facts
BEGIN
    SELECT RAISE(ABORT, 'entity_facts provenance fields are immutable after insert (decisions.id=546)');
END;

-- Require source_persona_id on every NEW row going forward (pre-migration
-- rows are exempt -- see backfill note above; this only governs inserts
-- from this point on).
CREATE TRIGGER IF NOT EXISTS trg_entity_facts_provenance_required
BEFORE INSERT ON entity_facts
WHEN NEW.source_persona_id IS NULL
BEGIN
    SELECT RAISE(ABORT, 'entity_facts.source_persona_id is required on insert (decisions.id=546)');
END;

INSERT OR IGNORE INTO schema_version (version, applied_at, description)
VALUES (7, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),
    'Cross-Persona Data Provenance (decisions.id=546): source_persona_id, cross_persona_export, origin_persona_id added to entity_facts with DB-enforced immutability; forked_from_instance_id/forked_at lineage columns added (non-gating, items.id=20 Q8)');
