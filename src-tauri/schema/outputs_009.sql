-- src-tauri/schema/outputs_009.sql
-- Migration 9: extend focus_runs.status CHECK to include awaiting_extract_confirm.
-- SQLite cannot ALTER CHECK constraints in place -- table recreation required.
--
-- New status value:
--   'awaiting_extract_confirm' -- post-execute extraction pass complete;
--                                 run parked pending user confirmation of
--                                 extracted personal fields (item 20).
--
-- Strategy: create focus_runs_new with extended CHECK, copy data, drop old,
-- rename new, recreate indexes and dependent table FKs.
--
-- Dependent tables referencing focus_runs(id) by FK:
--   focus_run_snapshots  -- ON DELETE CASCADE, heals by name after rename
--   outputs              -- no CASCADE, value-based FK only
--   model_quality_scores -- no CASCADE, value-based FK only
--   drift_observations   -- no CASCADE, value-based FK only
--   run_history          -- no CASCADE, value-based FK only
--   consent_decisions    -- no CASCADE, value-based FK only
--   extract_confirm_candidates -- ON DELETE CASCADE, heals by name after rename
--
-- FK integrity: SQLite enforces FKs only when PRAGMA foreign_keys=ON.
-- The migration runner does not set this PRAGMA, so no FK violations fire
-- during the swap. UUID values are identical in new table -- integrity by value.
--
-- Executed within run_migrations SAVEPOINT -- atomic or rolled back.

-- Step 1: create focus_runs_new with extended status CHECK
CREATE TABLE IF NOT EXISTS focus_runs_new (
    id                          TEXT PRIMARY KEY,
    focus_id                    TEXT NOT NULL,
    status                      TEXT NOT NULL DEFAULT 'initializing'
                                    CHECK (status IN (
                                        'initializing','running','paused',
                                        'awaiting_user','awaiting_feedback',
                                        'awaiting_extract_confirm',
                                        'complete','cancelled','failed'
                                    )),
    is_fast_lane                INTEGER NOT NULL DEFAULT 0,
    routing_tier_used           INTEGER,
    started_at                  TEXT NOT NULL,
    completed_at                TEXT,
    feedback_window_expires_at  TEXT,
    signal_validity             TEXT
                                    CHECK (signal_validity IS NULL OR
                                        signal_validity IN
                                            ('valid','partial','invalid')),
    notes                       TEXT NOT NULL DEFAULT '{}',
    extra_metadata              TEXT NOT NULL DEFAULT '{}',
    topic_id                    TEXT REFERENCES topics(id),
    is_quick_ask                INTEGER NOT NULL DEFAULT 0
);

-- Step 2: copy all existing rows
INSERT INTO focus_runs_new
    (id, focus_id, status, is_fast_lane, routing_tier_used,
     started_at, completed_at, feedback_window_expires_at,
     signal_validity, notes, extra_metadata, topic_id, is_quick_ask)
SELECT
    id, focus_id, status, is_fast_lane, routing_tier_used,
    started_at, completed_at, feedback_window_expires_at,
    signal_validity, notes, extra_metadata, topic_id, is_quick_ask
FROM focus_runs;

-- Step 3: drop old table (cascades focus_run_snapshots and
-- extract_confirm_candidates via ON DELETE CASCADE on their FK)
DROP TABLE IF EXISTS focus_runs;

-- Step 4: rename new table
ALTER TABLE focus_runs_new RENAME TO focus_runs;

-- Step 5: recreate indexes
CREATE INDEX IF NOT EXISTS idx_focus_runs_status
    ON focus_runs (status, started_at DESC);

CREATE INDEX IF NOT EXISTS idx_focus_runs_topic
    ON focus_runs (topic_id, started_at DESC)
    WHERE topic_id IS NOT NULL;

INSERT OR IGNORE INTO schema_version (version, applied_at, description)
VALUES (9, datetime('now'),
    'Extend focus_runs.status CHECK to include awaiting_extract_confirm (item 20)');
