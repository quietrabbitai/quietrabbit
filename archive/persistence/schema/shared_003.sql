-- persistence/schema/shared_003.sql
-- Phase B: adds topic_index and asset_index stub to shared.db.
-- topic_index: lightweight metadata pointer — no content.
-- Allows Life dashboard to surface active/paused topics across all focuses
-- without opening per-life encrypted databases.
-- asset_index: schema stub only — no CRUD store or UI in Release 1.
--   Activates in Layer 8+.
-- Part of Phase B data model extension (D6-226+).

-- Step 1: topic_index — Life-level pointer to active topics.
-- topic_id: references topics.id in outputs.db (cross-db FK by value only —
--   SQLite cannot enforce cross-database FK constraints).
-- lifecycle_state: mirrored from outputs.db topics table for dashboard queries.
--   outputs.db topics table is the authoritative source of truth.
--   topic_index.lifecycle_state is a cache copy — updated by Phase 5A and
--   Reconciliation Boot Check. On conflict, outputs.db governs.
-- display_name: resolved from topics.name OR topics.placeholder_name.
--   Never derived from user input content directly.
-- content_summary: NULL in Release 1. Phase 5B standing summary cache (R2+).
-- session_count: mirrored from plan_state.db topic_header — cache copy.
CREATE TABLE IF NOT EXISTS topic_index (
    topic_id            TEXT PRIMARY KEY,
    life_id             TEXT NOT NULL,
    focus_id            TEXT NOT NULL,
    display_name        TEXT NOT NULL,
    lifecycle_state     TEXT NOT NULL
                            CHECK (lifecycle_state IN (
                                'active', 'paused', 'awaiting',
                                'complete', 'closed'
                            )),
    last_active_at      TEXT NOT NULL,
    session_count       INTEGER NOT NULL DEFAULT 0,
    content_summary     TEXT,
    created_at          TEXT NOT NULL,
    updated_at          TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_topic_index_life
    ON topic_index (life_id, lifecycle_state, last_active_at DESC);

CREATE INDEX IF NOT EXISTS idx_topic_index_focus
    ON topic_index (focus_id, lifecycle_state);

-- Step 2: asset_index stub — schema only, no CRUD store in Release 1.
-- asset_id: UUID primary key.
-- life_id: Life-level scope for cross-focus access (Option B Space-level promotion).
-- focus_id: NULL = Life-level asset. Non-null = focus-scoped asset.
-- asset_type: 'static' (discrete named artifact) | 'structured' (schema + append).
-- backing_type: 'local' (R1) | 'imported' | 'connected' (R2+ only).
-- name_sensitivity: sensitivity preset of the asset name itself.
--   Names with medical/financial sensitivity obfuscated in public display —
--   forward to Chat-BRAND for UI specification.
-- content_ref: pointer to actual content location (path or DB ref). NULL in R1.
-- Asset index dual-write invariant (ADR-013 Section 2.4):
--   All mutations use atomic transaction locking shared.db index pointer
--   before content layer commits. Orphaned entries detected by Boot Check.
CREATE TABLE IF NOT EXISTS asset_index (
    asset_id            TEXT PRIMARY KEY,
    life_id             TEXT NOT NULL,
    focus_id            TEXT,
    asset_type          TEXT NOT NULL
                            CHECK (asset_type IN ('static', 'structured')),
    backing_type        TEXT NOT NULL DEFAULT 'local'
                            CHECK (backing_type IN ('local', 'imported', 'connected')),
    name                TEXT NOT NULL,
    name_sensitivity    TEXT NOT NULL DEFAULT 'standard'
                            CHECK (name_sensitivity IN (
                                'standard', 'sensitive', 'private', 'locked'
                            )),
    content_ref         TEXT,
    created_at          TEXT NOT NULL,
    last_modified_at    TEXT NOT NULL,
    extra_metadata      TEXT NOT NULL DEFAULT '{}'
);

CREATE INDEX IF NOT EXISTS idx_asset_index_life
    ON asset_index (life_id, focus_id, last_modified_at DESC);

INSERT OR IGNORE INTO schema_version (version, applied_at, description)
VALUES (3, datetime('now'),
    'Phase B: topic_index and asset_index stub tables');
