-- group_001.sql
--
-- items.id=283 (266a): group.db foundation schema. First of six sub-items
-- decomposing items.id=266 (household/business group sharing) -- see
-- Working/GROUP_DB_DESIGN_20260802.md Section 2 (group documents) and
-- decisions.id=210 (checkout-lock resolution, 2026-08-16). Data layer only
-- -- no CRUD, no permission enforcement, no fork logic (see per-table notes
-- below for exactly which later item owns each of those).
--
-- ONE group.db PER GROUP. Every table in this file is implicitly scoped to
-- the single group this file belongs to -- no group_id column appears on
-- any table here, matching the convention personal.db's own tables already
-- use (no persona_id column needed inside a file that's already
-- persona-scoped). The group's identity is carried by the file's path
-- (see migrate_group_db in migrations.rs) and by shared.db's
-- pending_group_invitations table (shared_003.sql), not by a row in here.
--
-- NOT PART OF ANY MEMBER'S ACCOUNT TREE (design doc Section 2.1): unlike
-- personal.db/outputs.db, group.db is deliberately NOT nested under
-- users/{user_id}/... -- see migrate_group_db's own path-construction
-- comment in migrations.rs.
--
-- SECURITY NOTE (design doc Section 2.2, restated here since it governs
-- how document_permissions below must be read): enforcement of these tiers
-- is APP-LAYER, not cryptographic. Anyone holding the group's symmetric
-- key can technically read every raw row in this file -- these tables
-- record trust-based sharing intent among people who already share the
-- group key (comparable to a shared Google Doc's sharing settings), NOT
-- the same security class as personal.db's cross-account isolation
-- guarantees. Real enforcement of these tiers against reads/writes is
-- items.id=285's scope, not built here.
--
-- SCHEMA AUTHORING RULE (migrations.rs): no semicolons inside string
-- literals -- parse_statements() is not a general-purpose SQL parser.

CREATE TABLE IF NOT EXISTS schema_version (
    version         INTEGER PRIMARY KEY,
    applied_at      TEXT NOT NULL,
    description     TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS migration_lock (
    id              INTEGER PRIMARY KEY CHECK (id = 1),
    locked_at       TEXT,
    locked_by       TEXT
);
INSERT OR IGNORE INTO migration_lock (id) VALUES (1);

-- documents (design doc Section 2.2/2.3) -- schema shell only. Actual
-- document content storage/read/write and real permission enforcement are
-- items.id=285's scope. Fork logic (canon-edit-or-fork choice, provenance-
-- tagged copy into personal.db) is items.id=286's scope -- deliberately
-- NOT represented here; see this migration's own handoff for why the
-- Section 2.3 fork-provenance fields (source group.db id, source document
-- id, canon version/timestamp forked from) have no table to attach to yet
-- and are not built in this item.
--
-- owner_persona_id: the document's single current owner (design doc
-- Section 2.4: "each document has exactly one owner at a time" -- this is
-- what keeps folder-sync's single-writer-per-document invariant tractable
-- without real distributed-systems machinery). Kept as its own column
-- rather than a tier='owner' row in document_permissions below, since a
-- single-valued fact recorded in two places would be two sources of truth
-- for the same thing -- document_permissions exists only for the other two
-- tiers, which really are per-member grants.
--
-- checked_out_by_persona_id / checked_out_at: items.id=210's resolution
-- (2026-08-16) -- stale-lock handling is MANUAL FORCE-UNLOCK ONLY, no
-- automatic timeout. Deliberately no expiry/heartbeat column: an
-- automatic-timeout design would need one, a manual-only design does not
-- -- force-unlock is just clearing these two columns, no extra schema
-- required for that (the clearing logic itself is items.id=285's scope).
-- checked_out_by_persona_id NULL means not checked out.
CREATE TABLE IF NOT EXISTS documents (
    id                          TEXT NOT NULL PRIMARY KEY,
    title                       TEXT NOT NULL,
    -- Pointer to actual content storage. Format/population is items.id=285's
    -- scope -- nothing in this item writes a real value here.
    content_ref                 TEXT,
    owner_persona_id            TEXT NOT NULL,
    checked_out_by_persona_id   TEXT,
    checked_out_at              TEXT,
    created_at                  TEXT NOT NULL,
    updated_at                  TEXT NOT NULL,
    extra_metadata              TEXT NOT NULL DEFAULT '{}'
                                    CHECK (json_valid(extra_metadata))
);

CREATE INDEX IF NOT EXISTS idx_documents_owner
    ON documents (owner_persona_id);

CREATE INDEX IF NOT EXISTS idx_documents_checked_out
    ON documents (checked_out_by_persona_id);

-- document_permissions -- per-document grants for the two non-owner tiers
-- (design doc Section 2.2: "Three tiers per document: owner, write,
-- read-only... any member can be promoted or demoted between all three
-- tiers"). 'owner' is intentionally NOT a valid value here -- see
-- documents.owner_persona_id's comment above for why ownership lives there
-- instead. A persona_id should not simultaneously appear here and as a
-- document's owner_persona_id; enforcing that is an application-layer
-- concern (items.id=285), consistent with this file's header note that all
-- tier enforcement in group.db is app-layer, not cryptographic/DB-level.
CREATE TABLE IF NOT EXISTS document_permissions (
    document_id     TEXT NOT NULL REFERENCES documents(id) ON DELETE CASCADE,
    persona_id      TEXT NOT NULL,
    tier            TEXT NOT NULL CHECK (tier IN ('write', 'read_only')),
    granted_at      TEXT NOT NULL,
    PRIMARY KEY (document_id, persona_id)
);

INSERT OR IGNORE INTO schema_version (version, applied_at, description)
VALUES (1, datetime('now'),
    'items.id=283 (266a): group.db foundation -- documents, document_permissions');
