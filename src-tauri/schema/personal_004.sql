-- personal_004.sql
--
-- items.id=286 (group.db 266d): canon-edit-or-fork -- the "fork to
-- personal" half of GROUP_DB_DESIGN_20260802.md Section 2.3. group_001.sql's
-- own header flagged this gap explicitly: the design doc says "fork to
-- personal.db," but personal.db had no document/content-shaped table to
-- receive one -- see document_fork_store.rs's own module doc for the full
-- fork_document() implementation this table backs.
--
-- SCHEMA LOCATION DECISION: personal.db, new table, not outputs.db.
-- outputs.db's `outputs` table (outputs_001.sql) is title/content-shaped on
-- the surface, but outputs.focus_run_id is `NOT NULL REFERENCES
-- focus_runs(id)` with no ON DELETE -- every row is Conductor-pipeline
-- output, not a direct user action. A forked document has no Focus run
-- behind it. Making focus_run_id nullable to accommodate this one new
-- row-type would ripple into every existing query against `outputs` that
-- assumes a real run -- model_quality_scores.focus_run_id and
-- drift_observations.focus_run_id are themselves NOT NULL too. personal.db
-- carries no such coupling and is the file the design doc's own wording
-- ("fork to personal") points at colloquially -- the user's own private,
-- non-group storage -- even though its existing tables (entities/
-- entity_facts) aren't document-shaped either. A new table is the clean fit.
--
-- NO owner_persona_id / persona_id COLUMN: personal.db is already one file
-- per (user_id, persona_id) pair -- path is
-- /users/{user_id}/personas/{persona_id}/personal.db. Every existing table
-- in this file (entities, entity_facts, voice_profiles, ...) omits a
-- persona_id column for the same reason (personal_001.sql's own header:
-- "user_id and persona_id are encoded in the file path"). document_forks
-- follows that convention rather than inventing an owner_persona_id column
-- the way group.db's documents table needs one (because that file can hold
-- documents owned by more than one Persona -- see group_001.sql). No
-- group.db-style permission tiers are needed here either: once forked, the
-- copy is wholly private to this Persona, full stop.
--
-- "VERSION" FIELD -- source_canon_updated_at, not an integer version
-- number: documents (group_001.sql) has no version column, only
-- updated_at, and update_document (group_store.rs) is the only write path
-- that changes an existing document's content -- it touches exactly
-- content_ref and updated_at, nothing else. updated_at is therefore the
-- only thing that actually changes when canon is edited, making it the
-- correct "version marker" design doc Section 2.3 asks for. Named
-- source_canon_updated_at rather than source_canon_version to avoid
-- implying an integer version scheme that does not exist anywhere in
-- group.db's schema.
--
-- NO INDEX on source_document_id: the only consumer of that column is the
-- deferred drift-notification feature (design doc Section 2.3, explicitly
-- R1+, explicitly out of scope here). Matches personal_003.sql's own
-- precedent -- no index without a real, currently-verified query path; add
-- one later if and when drift-notification actually ships.
--
-- SCHEMA AUTHORING RULE (migrations.rs): no semicolons inside string
-- literals -- parse_statements() is not a general-purpose SQL parser.

CREATE TABLE IF NOT EXISTS document_forks (
    id                          TEXT NOT NULL PRIMARY KEY,
    title                       TEXT NOT NULL,
    content                     TEXT NOT NULL,
    source_group_id             TEXT NOT NULL,
    source_document_id          TEXT NOT NULL,
    source_canon_updated_at     TEXT NOT NULL,
    created_at                  TEXT NOT NULL,
    updated_at                  TEXT NOT NULL,
    extra_metadata              TEXT NOT NULL DEFAULT '{}'
                                    CHECK (json_valid(extra_metadata))
);

INSERT OR IGNORE INTO schema_version (version, applied_at, description)
VALUES (4, datetime('now'),
    'items.id=286: document_forks -- clean-break, provenance-tagged personal.db copy of a group.db document (GROUP_DB_DESIGN_20260802.md Section 2.3)');
