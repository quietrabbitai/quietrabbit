-- shared_021.sql
--
-- items.id=496: concrete DDL for the Focus composition + customization
-- mechanism deferred since decisions.id=595 (storage concerns #2 "Focus
-- composition + customization" and #3 "Focus-level data"). Lives in
-- shared.db, not a new DB file and not qr_docs.db: Focus-TYPE data (not
-- per-user-instance data) is read at Phase 1 LOAD, before any encrypted
-- per-persona outputs.db opens -- the same constraint that already put
-- focus_settings here (D6-299, "Privacy Guardian must read Focus settings
-- before opening encrypted per-user stores... Settings are behavioral
-- config, not personal data").
--
-- DUAL-PATH, NOT A MIGRATION: conductor::lifecycle::load_focus_definition()
-- checks for a `focuses` row by focus_id first; if absent, it falls back to
-- today's .focus YAML file read, byte-for-byte unchanged. The 5 shipped
-- Focuses are not migrated into these tables by this file or any code this
-- item builds -- decisions.id=595 explicitly reserves "how the built five
-- map onto blocks" for the Phase-4 retrofit (items.id=129, blocked on this
-- item), so deciding that mapping now would preempt a question that
-- decision deliberately left open. Only new Focuses authored after this
-- lands get rows here.
--
-- building_blocks: a validation-only mirror of qr_docs.db's own
-- building_blocks catalog (Chat-PM's coordination DB -- confirmed live,
-- items.id=496's own investigation, never queried by the running app).
-- Rust's dispatch on a composition row's block_stable_id is a compiled-in
-- match statement, not driven by this table -- this table exists solely so
-- LOAD-time validation can reject a composition row citing an unknown or
-- non-inline_composable block_stable_id, the same role validate_step()
-- already plays for a malformed .focus YAML file. Seeded below with the 9
-- rows that are BOTH catalog_status='confirmed' AND
-- invocation_mode='inline_composable' in qr_docs.db as of 2026-09-26
-- (cb-01,02,03,05,06,07,08,10,11). Excluded, and why:
--   - cb-04, cb-09: invocation_mode='standing_gateway' -- boundary
--     interceptors (Tier2/3 promotion gate, Output Privacy Guardian scan),
--     already wired as fixed Conductor-lifecycle mechanisms, structurally
--     never placeable as a step in a composition sequence.
--   - pc-01..04: invocation_mode='inline_composable' same as the 9 seeded
--     rows -- NOT excluded for that reason. Excluded because
--     catalog_status='promotion_candidate' (block_kind='promoted_from_focus',
--     second_adopter_status='single_adopter') -- not yet past
--     decisions.id=594's second-adopter catalog-admission gate. Seeding a
--     promotion candidate here would let a Focus composition reference a
--     block the catalog itself hasn't confirmed yet.
-- None of the 9 seeded rows has real Rust execution code today (confirmed
-- live against conductor/executor.rs) -- seeding the mirror row is
-- authoring-time bookkeeping, independent of whether a handler exists yet;
-- execute_step()'s dispatch match is what actually enforces "no handler,
-- no run" (LOAD-time validation error, never a mid-run failure).
--
-- SCHEMA AUTHORING RULE (migrations.rs): no semicolons inside string
-- literals -- parse_statements() is not a general-purpose SQL parser.

CREATE TABLE IF NOT EXISTS building_blocks (
    stable_id        TEXT PRIMARY KEY,
    display_name     TEXT NOT NULL,
    invocation_mode  TEXT NOT NULL CHECK (invocation_mode IN
                          ('inline_composable', 'standing_gateway')),
    created_at       TEXT NOT NULL,
    updated_at       TEXT NOT NULL
);

INSERT OR IGNORE INTO building_blocks
    (stable_id, display_name, invocation_mode, created_at, updated_at)
VALUES
    ('cb-01', 'Entity-scoped structured record store + search', 'inline_composable', datetime('now'), datetime('now')),
    ('cb-02', 'Context assembly (broad/full)', 'inline_composable', datetime('now'), datetime('now')),
    ('cb-03', 'Document-to-structured-record extraction', 'inline_composable', datetime('now'), datetime('now')),
    ('cb-05', 'Confirm-before-save / staging', 'inline_composable', datetime('now'), datetime('now')),
    ('cb-06', 'Quality/completeness assessment', 'inline_composable', datetime('now'), datetime('now')),
    ('cb-07', 'Named re-entry with delta scope', 'inline_composable', datetime('now'), datetime('now')),
    ('cb-08', 'Two-axis proactive check-in', 'inline_composable', datetime('now'), datetime('now')),
    ('cb-10', 'Propose -> evaluate feedback -> route', 'inline_composable', datetime('now'), datetime('now')),
    ('cb-11', 'Source-of-truth / deduplication framework', 'inline_composable', datetime('now'), datetime('now'));

-- Focus-level data (decisions.id=595 concern #3). One row per Focus TYPE
-- (not per run/instance) -- focus_id is the same string key already used
-- throughout (focus_settings.focus_id, focus_runs.focus_id). Carries the
-- non-step fields of today's .focus YAML file's top level.
CREATE TABLE IF NOT EXISTS focuses (
    focus_id                            TEXT PRIMARY KEY,
    display_name                        TEXT NOT NULL,
    description                         TEXT NOT NULL DEFAULT '',
    version                             TEXT NOT NULL DEFAULT '1.0',
    max_routing_tier                    TEXT NOT NULL CHECK (max_routing_tier IN (
                                             'local_only', 'anonymous_required',
                                             'anonymous_preferred', 'unrestricted'
                                         )),
    output_type                         TEXT NOT NULL DEFAULT 'general',
    suggest_in_focuses                  TEXT NOT NULL DEFAULT '[]',
    multi_source_validation             INTEGER NOT NULL DEFAULT 0,
    generic_title_template              TEXT NOT NULL DEFAULT 'Hidden item',
    -- decisions.id=712 vocabulary, Focus-level (mirrors today's
    -- display_config.high_priority_trigger) -- unrelated to the
    -- per-composition-row schedule_trigger below.
    high_priority_trigger_anchor_field  TEXT,
    high_priority_trigger_offset        TEXT,
    status                              TEXT NOT NULL CHECK (status IN (
                                             'designed', 'in_build', 'shipped', 'deferred'
                                         )),
    created_at                          TEXT NOT NULL,
    updated_at                          TEXT NOT NULL
);

-- Focus composition (decisions.id=595 concern #2, part A): the ordered
-- block list. One row = one step-equivalent slot -- 1:1 with StepDefinition
-- -- so focus_run_steps/reenter_step()/rehydrate_focus_run()'s existing
-- (focus_run_id, step_id, sequence_index) keying needs no change.
-- block_stable_id NULL means "plain LLM-generate step", the same shape
-- every YAML-authored step has today -- not every real Focus step is
-- backed by a cataloged block (items.id=496's own status_detail leaves
-- this case explicitly open, unnamed, in the catalog itself).
CREATE TABLE IF NOT EXISTS focus_block_compositions (
    id                             TEXT PRIMARY KEY,
    focus_id                       TEXT NOT NULL REFERENCES focuses(focus_id) ON DELETE CASCADE,
    step_id                        TEXT NOT NULL,
    block_stable_id                TEXT REFERENCES building_blocks(stable_id),
    sequence_index                 INTEGER NOT NULL,
    -- decisions.id=712 vocabulary reused verbatim, placed per-composition-
    -- row (items.id=496 Q2's resolution) rather than per-Focus like
    -- focuses.high_priority_trigger_* above -- a deliberate divergence from
    -- that display-only trigger's own precedent, since this one gates
    -- execution eligibility for one specific step, not Active Board
    -- surfacing for the whole Focus instance.
    schedule_trigger_anchor_field  TEXT,
    schedule_trigger_offset        TEXT,
    created_at                     TEXT NOT NULL,
    updated_at                     TEXT NOT NULL,
    UNIQUE (focus_id, step_id),
    UNIQUE (focus_id, sequence_index)
);

CREATE INDEX IF NOT EXISTS idx_focus_block_compositions_focus
    ON focus_block_compositions (focus_id, sequence_index);

-- Focus customization (decisions.id=595 concern #2, part B): the parameter
-- payload for one composition row. Kept as its own table rather than a
-- column on focus_block_compositions -- decisions.id=591 treats "generic
-- core" vs "customization layer" as a structural split, and a separate
-- table leaves room for a later Persona-scoped override (a second row on
-- the same composition_id, non-default) without restructuring. No code
-- this item builds writes or reads a non-default (persona_id NOT NULL) row
-- -- representable, not yet real (decisions.id=594 second-adopter gating:
-- don't build for a hypothetical adopter).
--
-- customization's JSON shape is block_stable_id's concern: when NULL, it is
-- the same shape a .focus YAML step's fields have today (display_name,
-- guide_id, task_type, routing_tier, requires_user_handoff, step_type,
-- output_var, prompt_template, field_requirements, options_override) --
-- conductor::lifecycle deserializes it through the same per-step derivation
-- helper the YAML path uses. When non-NULL, the shape belongs to whatever
-- block's Rust handler eventually reads it -- open-ended at this layer,
-- same "schema intentionally open-ended, typed struct deferred" precedent
-- StepDefinition.options_override already established.
CREATE TABLE IF NOT EXISTS focus_block_customization (
    id              TEXT PRIMARY KEY,
    composition_id  TEXT NOT NULL REFERENCES focus_block_compositions(id) ON DELETE CASCADE,
    persona_id      TEXT REFERENCES personas(id) ON DELETE CASCADE,
    customization   TEXT NOT NULL DEFAULT '{}',
    created_at      TEXT NOT NULL,
    updated_at      TEXT NOT NULL,
    UNIQUE (composition_id, persona_id)
);

-- SQLite treats NULL as distinct under a UNIQUE constraint, so the
-- UNIQUE(composition_id, persona_id) column constraint above does NOT by
-- itself cap the Focus-type-default (persona_id IS NULL) row at one --
-- this partial index is the actual enforcement for that case.
CREATE UNIQUE INDEX IF NOT EXISTS idx_focus_block_customization_default
    ON focus_block_customization (composition_id)
    WHERE persona_id IS NULL;

CREATE INDEX IF NOT EXISTS idx_focus_block_customization_composition
    ON focus_block_customization (composition_id);

INSERT OR IGNORE INTO schema_version (version, applied_at, description)
VALUES (21, datetime('now'),
    'items.id=496: building_blocks (validation-only runtime mirror of qr_docs.db catalog), focuses, focus_block_compositions, focus_block_customization -- concrete DDL for the composition+customization mechanism decisions.id=591/594/595 deferred; dual-path with today''s YAML Focuses, no migration of the shipped 5');
