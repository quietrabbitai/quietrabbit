-- persistence/schema/personal_001.sql
-- Per-user, per-persona personal database schema: personal.db
-- Encrypted with SQLCipher using user master key.
-- Path: /users/{user_id}/personas/{persona_id}/personal.db
-- user_id and persona_id are encoded in the file path — not repeated
-- in most tables. Kept in disclosure_log only for audit trail integrity.
--
-- CONSOLIDATION NOTE (items.id=169, 2026-07-24): this file replaces the prior
-- seven-migration chain: personal_001 (initial, 'specialist_id'/'space_id'),
-- personal_002 (ADR-012 execution_tier/abstraction_tier on disclosure_log),
-- personal_003 (Phase A rename specialist_id->source_id, space_id->life_id,
-- path_run_id->focus_run_id, D6-224/225), personal_004 (Phase C Persona
-- migration life_id->persona_id, D6-289 through D6-303), personal_005 VOIDED
-- (D6-374, never applied/registered -- correctly absent from both the old
-- chain and this consolidation), personal_006 (Entity model: entities +
-- entity_facts replace personal_fields, D6-372), personal_007 (Cross-Persona
-- Data Provenance columns on entity_facts, decisions.id=546, items.id=27).
-- Pre-release, zero shipped users -- consolidated directly to final naming.
-- personal_fields never existed in this consolidated file: entities/
-- entity_facts are the only fact-storage tables, matching what a fresh
-- install actually ends up with after the full original chain runs.
-- Chat-DEV, per Chat-PM/Jason adjudication of Chat-DEV handoff id=99.
--
-- Field encryption note:
--   The entire DB is SQLCipher-encrypted at file level — no plaintext on disk.
--   HKDF per-field encryption (additional layer) activates in Layer 8.
--   The store API is encryption-agnostic — callers pass field_value as str.
--
-- disclosure_log is NEVER deleted — permanent audit trail (D6-198).
-- delete_disclosure_log does NOT exist in this module. Do not add it.
--
-- QUERY STYLE: runtime sqlx::query() only — no query!() macros.
-- PRAGMA key applied via SqliteConnectOptions (D6-346).

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
--   Singletons replace the retired personal_fields table and work
--   identically for context injection.
-- entity_id non-NULL = fact about a named entity (person, device, pet, etc.).
-- Two partial unique indexes enforce one active fact per (entity, field):
--   one for singletons, one for entity-scoped facts.
-- valid_from / valid_until: temporal validity (NULL = no bound).
-- source: provenance — 'interview', 'extract_confirm', 'user_edit', 'migrated', etc.
-- abstraction_tier2 / abstraction_tier3: available for future context injection policy.
-- sensitivity_severity: STORED generated column. STORED is required (not optional) —
--   SQLite cannot reliably index a VIRTUAL generated column across all versions.
-- Facts are immutable — updates are new rows. No updated_at column.
--
-- Cross-Persona Data Provenance (decisions.id=546, items.id=27):
--   source_persona_id, cross_persona_export, origin_persona_id are immutable
--   after insert (DB-enforced trigger below) — "unchangeable by any Conductor
--   action, Focus declaration, or user setting."
--   personal.db is one file per Persona INSTANCE (not per Persona type) --
--   path is /users/{user_id}/personas/{persona_id}/personal.db. Every row is
--   physically scoped to one instance already; source_persona_id records that
--   scope explicitly for the provenance check in decisions.id=424 context assembly.
--     source_persona_id    -- Persona INSTANCE (personas.id, shared.db) this
--                              row currently lives in. Required on every insert.
--     cross_persona_export -- 0 for every native/forked fact. 1 only when this
--                              row is a user-approved copy sitting in a Persona
--                              other than the one it originated in.
--     origin_persona_id    -- NULL unless cross_persona_export=1; names the
--                              Persona instance this fact originated in,
--                              permanently, for the required per-session
--                              re-confirmation UI (decisions.id=546).
--   NOT used for same-Persona-type instance forking (items.id=20 Q3/Q8):
--   forked facts are native to their new instance (cross_persona_export=0) --
--   forking within one Persona type is not a privacy-boundary crossing.
--
-- Lineage metadata for same-type instance forking (items.id=20 Q8):
--   forked_from_instance_id / forked_at. Nullable, non-gating -- does NOT
--   participate in the cross-Persona confirmation mechanism above. Populated
--   only by the fork-copy write path.
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS entity_facts (
    id                    TEXT PRIMARY KEY,
    entity_id             TEXT REFERENCES entities(id) ON DELETE CASCADE,
    field_name            TEXT NOT NULL,
    field_value           BLOB NOT NULL,
    sensitivity           TEXT NOT NULL
                              CHECK (sensitivity IN
                                  ('general', 'personal', 'medical', 'financial')),
    sensitivity_severity  INTEGER NOT NULL GENERATED ALWAYS AS (
                              CASE sensitivity
                                  WHEN 'general'   THEN 1
                                  WHEN 'personal'  THEN 2
                                  WHEN 'medical'   THEN 3
                                  WHEN 'financial' THEN 4
                                  ELSE 99
                              END
                          ) STORED,
    abstraction_tier2     TEXT NOT NULL DEFAULT 'pass'
                              CHECK (abstraction_tier2 IN
                                  ('pass', 'omit', 'summarize',
                                   'range_only', 'not_permitted')),
    abstraction_tier3     TEXT NOT NULL DEFAULT 'pass'
                              CHECK (abstraction_tier3 IN
                                  ('pass', 'omit', 'summarize',
                                   'range_only', 'not_permitted')),
    source                TEXT NOT NULL DEFAULT 'interview',
    valid_from            TEXT,
    valid_until           TEXT,
    created_at            TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    extra_metadata        TEXT NOT NULL DEFAULT '{}'
                              CHECK (json_valid(extra_metadata)),
    source_persona_id     TEXT,
    cross_persona_export  INTEGER NOT NULL DEFAULT 0
                              CHECK (cross_persona_export IN (0, 1)),
    origin_persona_id     TEXT
                              CHECK (origin_persona_id IS NULL
                                     OR cross_persona_export = 1),
    forked_from_instance_id TEXT,
    forked_at                TEXT
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

-- Immutability enforcement (decisions.id=546: "unchangeable by any Conductor
-- action, Focus declaration, or user setting"). DB-level, not a convention
-- every write path has to remember -- any UPDATE touching these three
-- columns after insert is rejected outright.
CREATE TRIGGER IF NOT EXISTS trg_entity_facts_provenance_immutable
BEFORE UPDATE OF source_persona_id, cross_persona_export, origin_persona_id
ON entity_facts
BEGIN
    SELECT RAISE(ABORT, 'entity_facts provenance fields are immutable after insert (decisions.id=546)');
END;

-- Require source_persona_id on every new row.
CREATE TRIGGER IF NOT EXISTS trg_entity_facts_provenance_required
BEFORE INSERT ON entity_facts
WHEN NEW.source_persona_id IS NULL
BEGIN
    SELECT RAISE(ABORT, 'entity_facts.source_persona_id is required on insert (decisions.id=546)');
END;

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

-- Voice profiles
-- precedence: 1=model_baseline 2=specialist_defaults 3=global
--             4=persona 5=writing_context (highest wins)
-- persona_id NULL = global (applies to all Personas).
-- source_id NULL = all sources.
CREATE TABLE IF NOT EXISTS voice_profiles (
    id              TEXT PRIMARY KEY,
    persona_id      TEXT,
    source_id       TEXT,
    precedence      INTEGER NOT NULL CHECK (precedence BETWEEN 1 AND 5),
    attribute       TEXT NOT NULL,
    value           TEXT NOT NULL,
    created_at      TEXT NOT NULL,
    updated_at      TEXT NOT NULL,
    extra_metadata  TEXT NOT NULL DEFAULT '{}'
);

CREATE INDEX IF NOT EXISTS idx_voice_profiles_lookup
    ON voice_profiles (source_id, precedence);

-- Disclosure log — NEVER deleted, permanent audit trail.
-- user_id retained here for audit trail integrity in backup/recovery.
-- execution_tier: model capability ceiling — which inference tier ran (ADR-012).
-- abstraction_tier: Gate1 field policy tier — how data was shaped (ADR-012).
--   Both nullable — pre-ADR-012 historical records never populated them;
--   all records going forward always populate both.
CREATE TABLE IF NOT EXISTS disclosure_log (
    id                  TEXT PRIMARY KEY,
    user_id             TEXT NOT NULL,
    persona_id          TEXT NOT NULL,
    focus_run_id        TEXT NOT NULL,
    step_id             TEXT NOT NULL,
    routing_tier        INTEGER NOT NULL,
    provider            TEXT,
    fields_shared       TEXT NOT NULL DEFAULT '[]',
    fields_abstracted   TEXT NOT NULL DEFAULT '{}',
    fields_withheld     TEXT NOT NULL DEFAULT '[]',
    override_declined   INTEGER NOT NULL DEFAULT 0,
    declined_at         TEXT,
    created_at          TEXT NOT NULL,
    extra_metadata      TEXT NOT NULL DEFAULT '{}',
    execution_tier      INTEGER,
    abstraction_tier    INTEGER
);

CREATE INDEX IF NOT EXISTS idx_disclosure_log_run
    ON disclosure_log (focus_run_id, created_at);

-- Staleness check state — single row per database (one per persona)
CREATE TABLE IF NOT EXISTS staleness_check_state (
    id              INTEGER PRIMARY KEY CHECK (id = 1),
    last_checked_at TEXT NOT NULL,
    fields_stale    TEXT NOT NULL DEFAULT '[]',
    check_result    TEXT NOT NULL DEFAULT 'ok'
                        CHECK (check_result IN ('ok','stale','error')),
    extra_metadata  TEXT NOT NULL DEFAULT '{}'
);

-- Notifications
CREATE TABLE IF NOT EXISTS notifications (
    id          TEXT PRIMARY KEY,
    severity    TEXT NOT NULL
                    CHECK (severity IN ('info','suggest','require','stop')),
    title       TEXT NOT NULL,
    body        TEXT NOT NULL,
    action_url  TEXT,
    read_at     TEXT,
    created_at  TEXT NOT NULL,
    extra_metadata TEXT NOT NULL DEFAULT '{}'
);

CREATE INDEX IF NOT EXISTS idx_notifications_unread
    ON notifications (read_at, created_at DESC);

INSERT OR IGNORE INTO schema_version (version, applied_at, description)
VALUES (1, datetime('now'),
    'personal.db schema (consolidated 2026-07-24, items.id=169): entities, entity_facts (with Cross-Persona Provenance, decisions.id=546), entity_relationships stub, voice_profiles, disclosure_log, staleness_check_state, notifications');
