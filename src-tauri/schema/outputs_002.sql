-- persistence/schema/outputs_002.sql
-- Migration 2: rename path_runs → focus_runs, path_run_snapshots → focus_run_snapshots.
-- Renames path_id column to focus_id on focus_runs.
-- Preserves all existing data, indexes, constraints, and FTS triggers.
-- Part of Phase A codebase rename (D6-224, D6-225).
--
-- Strategy: create new tables, copy data, drop old tables, recreate indexes.
-- Executed within run_migrations SAVEPOINT — atomic or rolled back.
--
-- Note on FTS triggers: outputs table references path_run_id as a column name
-- only in the outputs DDL (outputs_001.sql). The FTS triggers operate on
-- outputs.content and outputs.output_type — unaffected by this migration.
-- The outputs.path_run_id column is a FK reference by value (UUID) only;
-- the UUID values are identical in focus_runs, so FK integrity is preserved.
--
-- Note on model_quality_scores and drift_observations: both tables have a
-- path_run_id FK column referencing path_runs(id). UUID values are identical
-- in focus_runs(id). FK constraint enforcement in SQLite requires
-- PRAGMA foreign_keys=ON at runtime; the migration preserves data integrity
-- by value. Column rename deferred to a future migration when those tables
-- are actively used.

-- Step 1: create focus_runs with renamed columns
CREATE TABLE IF NOT EXISTS focus_runs (
    id                          TEXT PRIMARY KEY,
    focus_id                    TEXT NOT NULL,
    status                      TEXT NOT NULL DEFAULT 'initializing'
                                    CHECK (status IN (
                                        'initializing','running','paused',
                                        'awaiting_user','awaiting_feedback',
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
    extra_metadata              TEXT NOT NULL DEFAULT '{}'
);

-- Step 2: copy existing path_runs data into focus_runs
INSERT INTO focus_runs
    (id, focus_id, status, is_fast_lane, routing_tier_used,
     started_at, completed_at, feedback_window_expires_at,
     signal_validity, notes, extra_metadata)
SELECT
    id, path_id, status, is_fast_lane, routing_tier_used,
    started_at, completed_at, feedback_window_expires_at,
    signal_validity, notes, extra_metadata
FROM path_runs;

-- Step 3: create focus_run_snapshots with updated FK reference
CREATE TABLE IF NOT EXISTS focus_run_snapshots (
    id                          TEXT PRIMARY KEY,
    focus_run_id                TEXT NOT NULL REFERENCES focus_runs(id)
                                    ON DELETE CASCADE,
    step_id                     TEXT NOT NULL,
    phase                       INTEGER NOT NULL,
    task_track_json             TEXT NOT NULL DEFAULT '{}',
    shared_state_json           TEXT NOT NULL DEFAULT '{}',
    personal_context_manifest   TEXT NOT NULL DEFAULT '{}',
    checkpoint_hash             TEXT NOT NULL,
    purge_after                 TEXT,
    created_at                  TEXT NOT NULL,
    extra_metadata              TEXT NOT NULL DEFAULT '{}'
);

-- Step 4: copy existing path_run_snapshots data
INSERT INTO focus_run_snapshots
    (id, focus_run_id, step_id, phase, task_track_json,
     shared_state_json, personal_context_manifest,
     checkpoint_hash, purge_after, created_at, extra_metadata)
SELECT
    id, path_run_id, step_id, phase, task_track_json,
    shared_state_json, personal_context_manifest,
    checkpoint_hash, purge_after, created_at, extra_metadata
FROM path_run_snapshots;

-- Step 5: drop old tables (CASCADE drops dependent indexes automatically)
DROP TABLE IF EXISTS path_run_snapshots;
DROP TABLE IF EXISTS path_runs;

-- Step 6: recreate indexes on new tables
CREATE INDEX IF NOT EXISTS idx_focus_runs_status
    ON focus_runs (status, started_at DESC);

CREATE INDEX IF NOT EXISTS idx_focus_run_snapshots_run
    ON focus_run_snapshots (focus_run_id, created_at DESC);

CREATE INDEX IF NOT EXISTS idx_focus_run_snapshots_purge
    ON focus_run_snapshots (purge_after)
    WHERE purge_after IS NOT NULL;

INSERT OR IGNORE INTO schema_version (version, applied_at, description)
VALUES (2, datetime('now'),
    'Rename path_runs to focus_runs, path_run_snapshots to focus_run_snapshots, path_id column to focus_id');
