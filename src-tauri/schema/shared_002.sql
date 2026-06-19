-- persistence/schema/shared_002.sql
-- Migration 2: rename spaces → lives, user_spaces → user_lives.
-- Renames columns: space_type → life_type,
--   privacy_default_tier → life_privacy_default_tier on lives.
-- Updates context_groups: space_id column → life_id.
-- Updates artifact_versions: space_id → life_id column,
--   artifact_type 'specialist' → 'guide' or 'operator',
--   artifact_type 'path' → 'focus',
--   adds CHECK constraint on artifact_type.
-- Part of Phase A codebase rename (D6-224, D6-225).
--
-- Strategy: create new tables, copy data, drop old, recreate indexes.
-- Executed within run_migrations SAVEPOINT — atomic or rolled back.
--
-- artifact_type mapping:
--   specialist + artifact_id = 'personal-specialist' → operator
--   specialist + any other artifact_id               → guide
--   path                                              → focus
--   anything else                                     → unchanged
--
-- artifact_type CHECK constraint: ('guide', 'operator', 'focus')
--   'integration' intentionally excluded — add when integrations are built.

-- Step 1: create lives table (replaces spaces)
CREATE TABLE IF NOT EXISTS lives (
    id                          TEXT PRIMARY KEY,
    display_name                TEXT NOT NULL,
    life_type                   TEXT NOT NULL,
    life_privacy_default_tier   INTEGER NOT NULL DEFAULT 1,
    max_permitted_tier          INTEGER NOT NULL DEFAULT 1,
    created_at                  TEXT NOT NULL,
    extra_metadata              TEXT NOT NULL DEFAULT '{}',
    CHECK (max_permitted_tier >= life_privacy_default_tier)
);

-- Step 2: copy spaces data into lives
INSERT INTO lives
    (id, display_name, life_type, life_privacy_default_tier,
     max_permitted_tier, created_at, extra_metadata)
SELECT
    id, display_name, space_type, privacy_default_tier,
    max_permitted_tier, created_at, extra_metadata
FROM spaces;

-- Step 3: create user_lives table (replaces user_spaces)
CREATE TABLE IF NOT EXISTS user_lives (
    user_id     TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    life_id     TEXT NOT NULL REFERENCES lives(id) ON DELETE CASCADE,
    joined_at   TEXT NOT NULL,
    PRIMARY KEY (user_id, life_id)
);

-- Step 4: copy user_spaces data into user_lives
INSERT INTO user_lives (user_id, life_id, joined_at)
SELECT user_id, space_id, joined_at
FROM user_spaces;

-- Step 5: update context_groups — rename space_id column to life_id.
-- SQLite does not support ALTER TABLE RENAME COLUMN on versions before 3.25.
-- Recreate the table to be safe across all supported SQLite versions.
CREATE TABLE IF NOT EXISTS context_groups_new (
    id              TEXT PRIMARY KEY,
    display_name    TEXT NOT NULL,
    life_id         TEXT REFERENCES lives(id) ON DELETE CASCADE,
    created_at      TEXT NOT NULL,
    extra_metadata  TEXT NOT NULL DEFAULT '{}'
);

INSERT INTO context_groups_new
    (id, display_name, life_id, created_at, extra_metadata)
SELECT
    id, display_name, space_id, created_at, extra_metadata
FROM context_groups;

DROP TABLE IF EXISTS context_groups;

ALTER TABLE context_groups_new RENAME TO context_groups;

-- Step 6: update artifact_versions.
-- Renames space_id column → life_id.
-- Remaps artifact_type values: specialist → guide/operator, path → focus.
-- Adds CHECK constraint on artifact_type.
CREATE TABLE IF NOT EXISTS artifact_versions_new (
    artifact_type   TEXT NOT NULL
                        CHECK (artifact_type IN ('guide', 'operator', 'focus')),
    artifact_id     TEXT NOT NULL,
    scope           TEXT NOT NULL DEFAULT '_global',
    life_id         TEXT NOT NULL DEFAULT '_global',
    version         TEXT NOT NULL,
    trust_level     TEXT NOT NULL
                        CHECK (trust_level IN
                            ('official', 'reviewed', 'community', 'local_only')),
    revoked         INTEGER NOT NULL DEFAULT 0,
    installed_at    TEXT NOT NULL,
    extra_metadata  TEXT NOT NULL DEFAULT '{}',
    PRIMARY KEY (artifact_type, artifact_id, scope, life_id)
);

INSERT INTO artifact_versions_new
    (artifact_type, artifact_id, scope, life_id, version,
     trust_level, revoked, installed_at, extra_metadata)
SELECT
    CASE
        WHEN artifact_type = 'specialist'
             AND artifact_id = 'personal-specialist' THEN 'operator'
        WHEN artifact_type = 'specialist'             THEN 'guide'
        WHEN artifact_type = 'path'                   THEN 'focus'
        ELSE artifact_type
    END,
    artifact_id,
    scope,
    space_id,
    version,
    trust_level,
    revoked,
    installed_at,
    extra_metadata
FROM artifact_versions;

DROP TABLE IF EXISTS artifact_versions;

ALTER TABLE artifact_versions_new RENAME TO artifact_versions;

-- Step 7: drop old tables (user_lives and context_groups_new already handled above)
DROP TABLE IF EXISTS user_spaces;
DROP TABLE IF EXISTS spaces;

-- Step 8: recreate indexes
-- idx_users_single_primary already exists from migration 1 — IF NOT EXISTS is safe.
CREATE UNIQUE INDEX IF NOT EXISTS idx_users_single_primary
    ON users (is_primary) WHERE is_primary = 1;

INSERT OR IGNORE INTO schema_version (version, applied_at, description)
VALUES (2, datetime('now'),
    'Rename spaces to lives, user_spaces to user_lives, space_id to life_id, artifact_type specialist to guide/operator, path to focus');
