-- personal_002.sql
--
-- cb-11 foundation: source-of-truth / deduplication framework.
-- items.id=128, decisions.id=502 (D6-460), decisions.id=621 §11.
-- Second confirmed adopter: decisions.id=617 household synced grants, which
-- reconcile recipient and owner instances through this same framework.
--
-- THREE CHANGES:
--   1. entities rebuilt — status CHECK widened to the decisions.id=502
--      record-status set, 'retired' collapsed into 'archived', and a new
--      modification_state column added.
--   2. source_registry — one row per contributing source per Focus.
--   3. dedup_candidates — surfaced duplicate pairs awaiting user judgement.
--
-- STATUS COLLAPSE (Jason, 2026-07-25): personal_001 gave entities a
-- lifecycle status of active/retired/archived. decisions.id=502 requires
-- active/deleted_in_source/user_archived/user_deleted for the same records.
-- Rather than carry two overlapping status columns, the two sets are merged
-- into one: 'user_archived' and 'archived' are the same idea and keep the
-- shorter name; 'retired' was introduced by cb-01 this same day, has no
-- decisions.id=502 counterpart, and is collapsed into 'archived'. Resulting
-- set: active / archived / deleted_in_source / user_deleted.
--
-- WHY A TABLE REBUILD RATHER THAN A SIDE TABLE: SQLite cannot alter a CHECK
-- constraint in place, so widening the status set requires a rebuild. The
-- alternative considered and rejected was a side table keyed by entity_id.
-- It was rejected because entities holds no production data at this point
-- (nothing has shipped), making the rebuild a one-time cost at its cheapest,
-- whereas a side table imposes a permanent JOIN on the most common read path
-- ("active records, excluding those deleted in source") and makes an absent
-- row ambiguous between "not source-derived" and "row never written".
--
-- RENAME HAZARD, handled deliberately below: entity_facts carries
-- REFERENCES entities. Between DROP TABLE entities and the rename of the
-- replacement, that reference dangles, and modern SQLite re-parses the whole
-- schema during ALTER TABLE ... RENAME — which can either error out or
-- rewrite the reference to the temporary name. PRAGMA legacy_alter_table
-- suppresses that re-parse for the duration of the rename, which is the
-- documented workaround. PRAGMA foreign_keys is not enabled anywhere in this
-- codebase, so no FK enforcement is being bypassed by the drop itself.
-- A migration test asserts entity_facts still names `entities` afterward.
--
-- SCHEMA AUTHORING RULE (migrations.rs): no semicolons inside string
-- literals — parse_statements() is not a general-purpose SQL parser.

PRAGMA legacy_alter_table = ON;

-- ---------------------------------------------------------------------------
-- 0. source_registry — created FIRST, before the entities rebuild
-- ---------------------------------------------------------------------------
-- Must precede the entities rebuild: entities.source_registry_id references
-- this table, and SQLite resolves that reference while copying rows into the
-- rebuilt table. Full commentary on the table's design lives in section 2
-- below, where it reads in logical rather than execution order.

CREATE TABLE IF NOT EXISTS source_registry (
    id                  TEXT PRIMARY KEY,
    persona_id          TEXT NOT NULL,
    focus_slug          TEXT NOT NULL,
    source_type         TEXT NOT NULL,
    -- JSON, live sources only (post-R1). NULL for import and QR-origin
    -- sources. Never holds a credential: decisions.id=502's live-source
    -- work is post-R1 and standing_rules.id=50 keeps secrets out of any
    -- store Claude reads or writes.
    connection_config   TEXT
                            CHECK (connection_config IS NULL
                                   OR json_valid(connection_config)),
    last_imported_at    TEXT,
    -- live sources only (post-R1)
    last_synced_at      TEXT,
    status              TEXT NOT NULL DEFAULT 'active'
                            CHECK (status IN ('active', 'archived',
                                              'pending_refresh')),
    created_at          TEXT NOT NULL,
    extra_metadata      TEXT NOT NULL DEFAULT '{}'
                            CHECK (json_valid(extra_metadata))
);

CREATE INDEX IF NOT EXISTS idx_source_registry_focus
    ON source_registry (focus_slug, status);

-- ---------------------------------------------------------------------------
-- 1. entities rebuild
-- ---------------------------------------------------------------------------
-- modification_state (decisions.id=502) governs what happens to a record on
-- a user-triggered source refresh:
--   pristine      imported, unedited — source updates auto-accepted
--   user_modified edited in QR — source update surfaces a conflict for the
--                 user to resolve per field; user-generated data always kept
--   user_created  originated in QR (manual entry, URL ingestion, generation)
--                 — source updates never apply, QR is authoritative
-- Default is user_created: a record with no import origin is QR's own.

CREATE TABLE IF NOT EXISTS entities_v2 (
    id                  TEXT PRIMARY KEY,
    entity_type         TEXT NOT NULL,
    display_name        TEXT NOT NULL,
    aliases             TEXT NOT NULL DEFAULT '[]'
                            CHECK (json_valid(aliases)),
    parent_entity_id    TEXT REFERENCES entities(id) ON DELETE SET NULL
                            CHECK (parent_entity_id IS NULL
                                   OR parent_entity_id != id),
    status              TEXT NOT NULL DEFAULT 'active'
                            CHECK (status IN ('active', 'archived',
                                              'deleted_in_source',
                                              'user_deleted')),
    modification_state  TEXT NOT NULL DEFAULT 'user_created'
                            CHECK (modification_state IN
                                ('pristine', 'user_modified', 'user_created')),
    -- Which registered source this record came from. NULL for QR-origin
    -- records (manual entry, generation), which is the same population
    -- modification_state='user_created' describes. Needed because
    -- decisions.id=502 makes refresh per-source, not per-user: without this
    -- edge, "refresh this source" cannot find the records it owns, and
    -- source transition (newer source active, prior archived) cannot tell
    -- which records belong to which.
    source_registry_id  TEXT REFERENCES source_registry(id) ON DELETE SET NULL,
    -- The record's URL at its source, when it has one. This is
    -- decisions.id=502's first and highest-confidence dedup match strategy
    -- (source_url match -> surface as probable duplicate), and it is
    -- preserved on the winning record when a pair is resolved.
    source_url          TEXT,
    created_at          TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    extra_metadata      TEXT NOT NULL DEFAULT '{}'
                            CHECK (json_valid(extra_metadata))
);

INSERT INTO entities_v2
    (id, entity_type, display_name, aliases, parent_entity_id, status,
     modification_state, created_at, extra_metadata)
SELECT
    id, entity_type, display_name, aliases, parent_entity_id,
    CASE status WHEN 'retired' THEN 'archived' ELSE status END,
    'user_created',
    created_at, extra_metadata
FROM entities;

DROP TABLE entities;

ALTER TABLE entities_v2 RENAME TO entities;

CREATE INDEX IF NOT EXISTS idx_entities_type_status
    ON entities (entity_type, status);

CREATE INDEX IF NOT EXISTS idx_entities_modification_state
    ON entities (modification_state);

-- Per-source refresh and source transition both scan by source.
CREATE INDEX IF NOT EXISTS idx_entities_source
    ON entities (source_registry_id, status);

-- decisions.id=502 dedup match strategy 1: source_url match.
CREATE INDEX IF NOT EXISTS idx_entities_source_url
    ON entities (source_url);

PRAGMA legacy_alter_table = OFF;

-- ---------------------------------------------------------------------------
-- 2. source_registry (decisions.id=502)
-- ---------------------------------------------------------------------------
-- One row per contributing source per Focus. A user's collection may span
-- several sources accumulated over time; QR does not assume one source of
-- truth per user. Refresh cadence is per-source, not per-user.
--
-- persona_id is retained from decisions.id=502's DDL even though personal.db
-- is already one file per Persona instance — it keeps rows self-describing
-- in exports and backups, matching entity_facts.source_persona_id's
-- reasoning (decisions.id=546).
--
-- source_type is deliberately FREE TEXT with no CHECK constraint.
-- decisions.id=502 enumerates: mealie_live, mealie_import, paprika_import,
-- bookmark_import, pdf_import, url_ingestion, user_created, qr_generated.
-- decisions.id=617 (household synced grants) requires an additional value
-- naming another QR Persona instance as a source, which 502 never
-- enumerated. That value is NOT invented here: instance-sharing
-- architecture is items.id=147 item 4, a separate Chat-DEV feasibility pass
-- not dispatched with this work. Leaving the column unconstrained keeps
-- adding it a data change rather than a migration.
--
-- Timestamps are TEXT RFC3339, not decisions.id=502's literal INTEGER —
-- every other timestamp column in personal.db is TEXT, and internal
-- consistency wins over the DDL sketch in the decision.

-- DDL ORDERING: this table is created at the TOP of the file, ahead of the
-- entities rebuild, not here. entities.source_registry_id references it, and
-- SQLite resolves that reference while copying rows into the rebuilt table —
-- it fails with "no such table: main.source_registry" if the target does not
-- exist yet. The commentary above remains the authoritative description of
-- the table; only the statement moved.

-- ---------------------------------------------------------------------------
-- 3. dedup_candidates (decisions.id=502)
-- ---------------------------------------------------------------------------
-- Surfaced duplicate pairs awaiting the user's judgement. The framework
-- never merges, never discards, and never acts autonomously — the user is
-- the sole authority on whether two records are the same thing.
--
-- record_id_a / record_id_b reference entities(id). No FK is declared:
-- PRAGMA foreign_keys is off codebase-wide, so a declared constraint would
-- be decorative, and a resolved candidate must outlive the record it
-- tombstoned. Referential integrity is enforced in the store layer.
--
-- match_confidence is advisory ONLY. decisions.id=502 is explicit that high
-- confidence does not mean the records are the same: one key field
-- difference (yeast vs unleavened, butter vs oil) makes a categorically
-- different record. differing_fields is what the user actually reviews.
--
-- The pair (record_id_a, record_id_b) is unique regardless of order — the
-- store layer normalises the two ids before insert so that (A,B) and (B,A)
-- cannot both accumulate as separate pending candidates.

CREATE TABLE IF NOT EXISTS dedup_candidates (
    id                  TEXT PRIMARY KEY,
    focus_slug          TEXT NOT NULL,
    record_id_a         TEXT NOT NULL,
    record_id_b         TEXT NOT NULL,
    match_confidence    REAL NOT NULL,
    match_basis         TEXT NOT NULL
                            CHECK (match_basis IN ('url_match', 'name_match',
                                                   'name_and_field_overlap')),
    -- JSON array of field names that differ between the two records
    differing_fields    TEXT
                            CHECK (differing_fields IS NULL
                                   OR json_valid(differing_fields)),
    status              TEXT NOT NULL DEFAULT 'pending'
                            CHECK (status IN ('pending',
                                              'user_confirmed_distinct',
                                              'resolved_keep_a',
                                              'resolved_keep_b')),
    created_at          TEXT NOT NULL,
    resolved_at         TEXT,
    CHECK (record_id_a != record_id_b)
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_dedup_candidates_pair
    ON dedup_candidates (focus_slug, record_id_a, record_id_b);

CREATE INDEX IF NOT EXISTS idx_dedup_candidates_pending
    ON dedup_candidates (focus_slug, status);

INSERT OR IGNORE INTO schema_version (version, applied_at, description)
VALUES (2, datetime('now'),
    'cb-11 source-of-truth / deduplication framework (items.id=128, decisions.id=502): entities rebuilt with widened record status and modification_state, source_registry, dedup_candidates');
