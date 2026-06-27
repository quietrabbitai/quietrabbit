-- schema/personal_006.sql
-- Entity model migration for personal.db
-- Replaces the flat personal_fields table with entities + entity_facts.
-- personal_005.sql is VOIDED (D6-374) — never applied, never registered.
-- Migration version: 6

-- ---------------------------------------------------------------------------
-- entities table
-- Represents named things in the user's world that accumulate facts.
-- entity_id is a TEXT UUID (application-generated).
-- parent_entity_id: self-referential for hierarchical groupings (e.g. family).
--   Self-parenting is blocked by CHECK constraint (prevents trivial cycles).
-- entity_type: open vocabulary — no CHECK constraint; extensible post-R1.
-- aliases: JSON array of alternate names (e.g. ["Dad", "Robert"]).
-- status: active / retired / archived lifecycle.
-- updated_at: deferred to a future migration when an update path exists.
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS entities (
    id                  TEXT PRIMARY KEY,
    entity_type         TEXT NOT NULL,
    display_name        TEXT NOT NULL,
    aliases             TEXT NOT NULL DEFAULT '[]'
                            CHECK (json_valid(aliases)),
    parent_entity_id    TEXT REFERENCES entities(id) ON DELETE SET NULL
                            CHECK (parent_entity_id IS NULL
                                   OR parent_entity_id != id),
    status              TEXT NOT NULL DEFAULT 'active'
                            CHECK (status IN ('active', 'retired', 'archived')),
    created_at          TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    extra_metadata      TEXT NOT NULL DEFAULT '{}'
                            CHECK (json_valid(extra_metadata))
);

CREATE INDEX IF NOT EXISTS idx_entities_type_status
    ON entities (entity_type, status);

-- ---------------------------------------------------------------------------
-- entity_facts table
-- One row per (entity, field_name) active fact.
-- entity_id NULL = singleton (user-level fact, no named entity owner).
--   Singletons replace personal_fields and work identically for context injection.
-- entity_id non-NULL = fact about a named entity (person, device, pet, etc.).
-- Two partial unique indexes enforce one active fact per (entity, field):
--   one for singletons, one for entity-scoped facts.
-- valid_from / valid_until: temporal validity (NULL = no bound).
-- source: provenance — 'interview', 'extract_confirm', 'user_edit', 'migrated', etc.
-- abstraction_tier2 / abstraction_tier3: available for future context injection policy.
-- sensitivity_severity: STORED generated column. STORED is required (not optional) —
--   SQLite cannot reliably index a VIRTUAL generated column across all versions.
-- Facts are immutable — updates are new rows. No updated_at column.
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS entity_facts (
    id                  TEXT PRIMARY KEY,
    entity_id           TEXT REFERENCES entities(id) ON DELETE CASCADE,
    field_name          TEXT NOT NULL,
    field_value         BLOB NOT NULL,
    sensitivity         TEXT NOT NULL
                            CHECK (sensitivity IN
                                ('general', 'personal', 'medical', 'financial')),
    sensitivity_severity INTEGER NOT NULL GENERATED ALWAYS AS (
                            CASE sensitivity
                                WHEN 'general'   THEN 1
                                WHEN 'personal'  THEN 2
                                WHEN 'medical'   THEN 3
                                WHEN 'financial' THEN 4
                                ELSE 99
                            END
                        ) STORED,
    abstraction_tier2   TEXT NOT NULL DEFAULT 'pass'
                            CHECK (abstraction_tier2 IN
                                ('pass', 'omit', 'summarize',
                                 'range_only', 'not_permitted')),
    abstraction_tier3   TEXT NOT NULL DEFAULT 'pass'
                            CHECK (abstraction_tier3 IN
                                ('pass', 'omit', 'summarize',
                                 'range_only', 'not_permitted')),
    source              TEXT NOT NULL DEFAULT 'interview',
    valid_from          TEXT,
    valid_until         TEXT,
    created_at          TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    extra_metadata      TEXT NOT NULL DEFAULT '{}'
                            CHECK (json_valid(extra_metadata))
);

-- One active fact per field_name for singletons (entity_id IS NULL)
CREATE UNIQUE INDEX IF NOT EXISTS idx_entity_facts_singleton_field
    ON entity_facts (field_name)
    WHERE entity_id IS NULL AND valid_until IS NULL;

-- One active fact per (entity_id, field_name) for entity-scoped facts
CREATE UNIQUE INDEX IF NOT EXISTS idx_entity_facts_entity_field
    ON entity_facts (entity_id, field_name)
    WHERE entity_id IS NOT NULL AND valid_until IS NULL;

CREATE INDEX IF NOT EXISTS idx_entity_facts_entity_id
    ON entity_facts (entity_id, sensitivity_severity);

-- ---------------------------------------------------------------------------
-- entity_relationships (stub — R1: no reads, no writes, no IPC)
-- Reserved for post-R1 relationship modelling between entities.
-- Uniqueness constraint prevents duplicate edges from accumulating.
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS entity_relationships (
    id                  TEXT PRIMARY KEY,
    from_entity_id      TEXT NOT NULL REFERENCES entities(id) ON DELETE CASCADE,
    to_entity_id        TEXT NOT NULL REFERENCES entities(id) ON DELETE CASCADE,
    relationship_type   TEXT NOT NULL,
    created_at          TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    extra_metadata      TEXT NOT NULL DEFAULT '{}'
                            CHECK (json_valid(extra_metadata))
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_entity_relationships_unique
    ON entity_relationships (from_entity_id, to_entity_id, relationship_type);

-- ---------------------------------------------------------------------------
-- Migrate personal_fields rows → entity_facts as singletons (entity_id = NULL).
-- Columns discarded: specialist_id, ownership_scope, abstraction_tier2,
--   abstraction_tier3, updated_at, extra_metadata (see D6-372).
-- source hardcoded to 'migrated' for provenance.
-- Fails loudly on any conflict — never silently discards user data.
-- ---------------------------------------------------------------------------
INSERT INTO entity_facts (
    id,
    entity_id,
    field_name,
    field_value,
    sensitivity,
    source,
    valid_from,
    valid_until,
    created_at
)
SELECT
    id,
    NULL,
    field_name,
    field_value,
    sensitivity,
    'migrated',
    NULL,
    NULL,
    created_at
FROM personal_fields;

-- ---------------------------------------------------------------------------
-- Drop personal_fields and its dependents.
-- personal_field_groups foreign-keys into personal_fields; drop it first.
-- Both drops are inside the migration SAVEPOINT — rolled back atomically
-- if any prior statement fails (see migrations.rs SAVEPOINT semantics).
-- ---------------------------------------------------------------------------
DROP TABLE IF EXISTS personal_field_groups;
DROP TABLE IF EXISTS personal_fields;

INSERT OR IGNORE INTO schema_version (version, applied_at, description)
VALUES (6, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),
    'Entity model: entities + entity_facts, migrate from personal_fields');
