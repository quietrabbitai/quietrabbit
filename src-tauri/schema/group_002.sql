-- group_002.sql
--
-- items.id=296: group_facts. GROUP_DB_DESIGN_20260802.md Section 3
-- ("group facts" -- read automatically by Conductor's context-assembly
-- layer at Focus-run time, distinct from Section 2's Library-shaped group
-- documents). Own versioned file, not folded into group_001.sql: group_001
-- is a v1 schema file (always re-run, must stay solely idempotent
-- statements per migrations.rs's validate_v1_rerun_safety) and its own
-- header already scopes itself to the six items.id=266 document sub-items
-- -- group facts is a separate item (296) with a different owner-write
-- shape, same reasoning personal_002..005 and shared_002..006 each used
-- their own version file for a genuinely new table rather than amending an
-- already-shipped v1 file.
--
-- NO entity_id / parent-entity concept (per this item's brief): group
-- facts are flat, not tied to any entity graph -- unlike personal.db's
-- entity_facts (personal_001.sql), which is entity-scoped or singleton.
--
-- FIELD SHAPE mirrors entity_facts (personal_001.sql) for the columns that
-- carry over: field_name, field_value, sensitivity (+ sensitivity_severity,
-- same GENERATED CASE), abstraction_tier2/tier3, extra_metadata.
-- Deliberately DOES NOT mirror entity_facts' valid_from/valid_until
-- temporal-versioning columns: group facts are owner-curated, single-
-- current-value rows edited by direct UPDATE, the same shape
-- documents.content_ref (group_001.sql) already uses -- there is no
-- supersede-and-keep-history model here. Also omits entity_facts'
-- source_persona_id/cross_persona_export/origin_persona_id provenance
-- columns entirely: those exist for the decisions.id=424/546 cross-Persona
-- confirmation check, which has no equivalent for group facts (Section
-- 3.3 point 4 -- "no cross-persona-export concept applies to group
-- facts"). No `source` (manual vs. promoted) provenance column either --
-- out of scope for this item; add it in a future group_003.sql if/when
-- promotion-from-personal-fact is actually designed, rather than guessing
-- its shape now.
--
-- owner_persona_id: DATA LAYER ONLY here -- no CRUD, no write enforcement
-- built by this item (items.id=296 wires the READ side into
-- build_personal_track() only; see persistence::group_fact_store's own
-- header). Included now, unenforced, for the same reason
-- documents.owner_persona_id (group_001.sql) shipped ahead of its own
-- enforcement in items.id=285 -- so the later manual-entry/owner-write item
-- doesn't need a schema migration just to add the column it obviously
-- needs. PER-ROW ownership (like documents), not one fixed group-wide
-- admin/owner: this codebase has no group-admin/role concept anywhere
-- (auth::group_membership.rs's own header: "deliberately NOT a durable
-- membership table") to hang a single owner-per-group model on, and a
-- future promotion-from-personal-fact flow is inherently per-member (each
-- member promotes their own personal facts), which only a per-row owner
-- shape supports.
--
-- READ ACCESS: unconditional for anyone holding the group's symmetric key
-- (Section 3.3 point 4 -- "the resident-key check... combined with the
-- opt-in, IS the complete gate. No second stacked check"). Unlike
-- group_001.sql's documents/document_permissions, there is no per-row
-- read-grant table for group_facts -- holding the group key is sufficient
-- to read every row here, by design.
--
-- SECURITY NOTE (restated from group_001.sql, applies identically here):
-- enforcement of any tier/ownership recorded in this file is APP-LAYER,
-- not cryptographic. Anyone holding the group's symmetric key can
-- technically read or (absent app-layer enforcement, not yet built)
-- write every raw row.
--
-- SCHEMA AUTHORING RULE (migrations.rs): no semicolons inside string
-- literals -- parse_statements() is not a general-purpose SQL parser.

CREATE TABLE IF NOT EXISTS group_facts (
    id                      TEXT NOT NULL PRIMARY KEY,
    owner_persona_id        TEXT NOT NULL,
    field_name              TEXT NOT NULL,
    field_value             BLOB NOT NULL,
    sensitivity              TEXT NOT NULL
                                CHECK (sensitivity IN
                                    ('general', 'personal', 'medical', 'financial')),
    sensitivity_severity     INTEGER NOT NULL GENERATED ALWAYS AS (
                                CASE sensitivity
                                    WHEN 'general'   THEN 1
                                    WHEN 'personal'  THEN 2
                                    WHEN 'medical'   THEN 3
                                    WHEN 'financial' THEN 4
                                    ELSE 99
                                END
                            ) STORED,
    abstraction_tier2       TEXT NOT NULL DEFAULT 'pass'
                                CHECK (abstraction_tier2 IN
                                    ('pass', 'omit', 'summarize',
                                     'range_only', 'not_permitted')),
    abstraction_tier3       TEXT NOT NULL DEFAULT 'pass'
                                CHECK (abstraction_tier3 IN
                                    ('pass', 'omit', 'summarize',
                                     'range_only', 'not_permitted')),
    created_at              TEXT NOT NULL,
    updated_at              TEXT NOT NULL,
    extra_metadata           TEXT NOT NULL DEFAULT '{}'
                                CHECK (json_valid(extra_metadata))
);

CREATE INDEX IF NOT EXISTS idx_group_facts_owner
    ON group_facts (owner_persona_id);

INSERT OR IGNORE INTO schema_version (version, applied_at, description)
VALUES (2, datetime('now'),
    'items.id=296: group_facts -- owner-curated facts read into PersonalTrack at Focus-run time (GROUP_DB_DESIGN_20260802.md Section 3)');
