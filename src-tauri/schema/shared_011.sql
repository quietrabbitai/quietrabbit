-- shared_011.sql
--
-- items.id=321 / decisions.id=729 -- extends the friction gate (items.id=92)
-- to also cover max_permitted_tier loosening, alongside the existing
-- privacy_tier / focus_profile->protected coverage. This file only touches
-- focus_settings_friction_decisions' audit shape; focus_settings itself
-- already has max_permitted_tier (shared_001.sql).
--
-- WHY A TABLE REBUILD, NOT ALTER TABLE ADD COLUMN (see shared_008/009/010
-- for the ADD COLUMN pattern used elsewhere): focus_settings_friction_
-- decisions carries a table-level CHECK enforcing "at least one requested_*
-- column is set" (shared_002.sql). SQLite's ALTER TABLE cannot add to or
-- otherwise modify an existing CHECK constraint -- only a full rebuild
-- (CREATE new -> copy -> DROP old -> RENAME) can widen it to a three-way OR
-- that also accepts requested_max_permitted_tier. Runs inside run_pending's
-- per-version SAVEPOINT (migrations.rs), so this is atomic with the rest of
-- this file.
--
-- existing_max_permitted_tier is NULLable, unlike existing_privacy_tier /
-- existing_focus_profile (both NOT NULL): every row written before this
-- migration has no real "max_permitted_tier at decision time" to backfill,
-- and fabricating one (e.g. from existing_privacy_tier, or today's
-- focus_settings value) would be dishonest data. Rows written after this
-- migration always have it populated by record_friction_gate_decision's
-- Rust caller (commands/consent.rs) -- NOT NULL in practice going forward,
-- just not enforceable in the schema for rows that predate the column.
--
-- SCHEMA AUTHORING RULE (migrations.rs): no semicolons inside string
-- literals -- parse_statements() is not a general-purpose SQL parser.

CREATE TABLE focus_settings_friction_decisions_new (
    id                            TEXT    PRIMARY KEY,
    persona_id                    TEXT    NOT NULL REFERENCES personas(id) ON DELETE CASCADE,
    focus_id                      TEXT    NOT NULL,
    decision                      TEXT    NOT NULL CHECK (decision IN ('proceed', 'cancel')),
    requested_privacy_tier        INTEGER CHECK (requested_privacy_tier IS NULL
                                                  OR requested_privacy_tier BETWEEN 1 AND 3),
    requested_focus_profile       TEXT    CHECK (requested_focus_profile IS NULL
                                                  OR requested_focus_profile IN
                                                      ('open', 'organized', 'protected')),
    requested_max_permitted_tier  INTEGER CHECK (requested_max_permitted_tier IS NULL
                                                  OR requested_max_permitted_tier BETWEEN 1 AND 3),
    -- Privacy tier / focus_profile / max_permitted_tier at the moment the
    -- gate fired -- preserved so an audit reader can see what the user
    -- moved away from, not only what they moved to.
    existing_privacy_tier         INTEGER NOT NULL CHECK (existing_privacy_tier BETWEEN 1 AND 3),
    existing_focus_profile        TEXT    NOT NULL CHECK (existing_focus_profile IN
                                                  ('open', 'organized', 'protected')),
    existing_max_permitted_tier   INTEGER CHECK (existing_max_permitted_tier IS NULL
                                                  OR existing_max_permitted_tier BETWEEN 1 AND 3),
    created_at                    TEXT    NOT NULL,

    -- At least one of the three requested_* fields must be present -- a row
    -- with none would mean the gate fired for no reason.
    CHECK (requested_privacy_tier IS NOT NULL
           OR requested_focus_profile IS NOT NULL
           OR requested_max_permitted_tier IS NOT NULL)
);

INSERT INTO focus_settings_friction_decisions_new
    (id, persona_id, focus_id, decision, requested_privacy_tier,
     requested_focus_profile, requested_max_permitted_tier,
     existing_privacy_tier, existing_focus_profile, existing_max_permitted_tier,
     created_at)
SELECT id, persona_id, focus_id, decision, requested_privacy_tier,
       requested_focus_profile, NULL,
       existing_privacy_tier, existing_focus_profile, NULL,
       created_at
FROM focus_settings_friction_decisions;

DROP TABLE focus_settings_friction_decisions;

ALTER TABLE focus_settings_friction_decisions_new
    RENAME TO focus_settings_friction_decisions;

CREATE INDEX IF NOT EXISTS idx_focus_settings_friction_decisions_lookup
    ON focus_settings_friction_decisions (persona_id, focus_id, created_at DESC);

INSERT OR IGNORE INTO schema_version (version, applied_at, description)
VALUES (11, datetime('now'),
    'items.id=321: focus_settings_friction_decisions rebuilt -- adds requested_max_permitted_tier/existing_max_permitted_tier, widens at-least-one CHECK to cover max_permitted_tier loosening');
