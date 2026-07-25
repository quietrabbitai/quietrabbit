-- persistence/schema/outputs_005.sql
-- Migration 5: rename path_run_id → focus_run_id in outputs,
-- model_quality_scores, and drift_observations.
-- Closes the deferral noted in outputs_002.sql.
-- Part of Phase A rename cleanup (D6-224, D6-225).
--
-- Strategy: recreate each table with renamed column, copy data,
-- drop old table, rename new table, recreate indexes.
-- Matches pattern in outputs_002.sql and outputs_004.sql.
--
-- Dependency: focus_runs table must exist (created in outputs_002.sql
-- migration 2). Migration runner applies versions in order — safe.
--
-- FTS5 note: outputs_fts triggers reference outputs columns by name.
-- Triggers are dropped before the table swap and recreated after.
-- FTS5 virtual table and shadow tables are preserved across the rename.
-- content='outputs' binding reattaches correctly after rename.
--
-- Generated column note: sensitivity_severity is GENERATED ALWAYS AS STORED.
-- It is omitted from INSERT...SELECT — SQLite recomputes it on insert.
--
-- Executed within run_migrations SAVEPOINT — atomic or rolled back.

-- Step 1: recreate outputs with focus_run_id (was path_run_id)
-- Drop FTS5 triggers first — they reference the old table by column name.
DROP TRIGGER IF EXISTS outputs_fts_insert;
DROP TRIGGER IF EXISTS outputs_fts_update;
DROP TRIGGER IF EXISTS outputs_fts_delete;

CREATE TABLE IF NOT EXISTS outputs_new (
    id                      TEXT PRIMARY KEY,
    focus_run_id            TEXT NOT NULL REFERENCES focus_runs(id),
    output_type             TEXT NOT NULL,
    content                 TEXT,
    content_pre_validation  TEXT,
    content_post_validation TEXT,
    sensitivity             TEXT NOT NULL DEFAULT 'general'
                                CHECK (sensitivity IN
                                    ('general','personal','medical','financial')),
    sensitivity_severity    INTEGER NOT NULL GENERATED ALWAYS AS (
                                CASE sensitivity
                                    WHEN 'general'   THEN 1
                                    WHEN 'personal'  THEN 2
                                    WHEN 'medical'   THEN 3
                                    WHEN 'financial' THEN 4
                                    ELSE 99
                                END
                            ) STORED,
    validation_provider     TEXT,
    validation_delta        TEXT,
    quality_rating          INTEGER,
    status                  TEXT NOT NULL DEFAULT 'active'
                                CHECK (status IN ('active','deleted')),
    created_at              TEXT NOT NULL,
    updated_at              TEXT NOT NULL,
    purge_scheduled_at      TEXT,
    purge_attempted_at      TEXT,
    purge_attempts          INTEGER NOT NULL DEFAULT 0,
    purged_at               TEXT,
    extra_metadata          TEXT NOT NULL DEFAULT '{}'
);

INSERT INTO outputs_new
    (id, focus_run_id, output_type, content,
     content_pre_validation, content_post_validation,
     sensitivity, validation_provider, validation_delta,
     quality_rating, status, created_at, updated_at,
     purge_scheduled_at, purge_attempted_at, purge_attempts,
     purged_at, extra_metadata)
SELECT
    id, path_run_id, output_type, content,
    content_pre_validation, content_post_validation,
    sensitivity, validation_provider, validation_delta,
    quality_rating, status, created_at, updated_at,
    purge_scheduled_at, purge_attempted_at, purge_attempts,
    purged_at, extra_metadata
FROM outputs;

DROP TABLE IF EXISTS outputs;
ALTER TABLE outputs_new RENAME TO outputs;

CREATE INDEX IF NOT EXISTS idx_outputs_focus_run
    ON outputs (focus_run_id, created_at DESC);

CREATE INDEX IF NOT EXISTS idx_outputs_active
    ON outputs (status, created_at DESC)
    WHERE status = 'active';

-- Recreate FTS5 triggers against renamed table
CREATE TRIGGER IF NOT EXISTS outputs_fts_insert
    AFTER INSERT ON outputs BEGIN
    INSERT INTO outputs_fts(rowid, content, output_type)
    VALUES (new.rowid, COALESCE(new.content,''), COALESCE(new.output_type,''));
END;

CREATE TRIGGER IF NOT EXISTS outputs_fts_update
    AFTER UPDATE ON outputs BEGIN
    INSERT INTO outputs_fts(outputs_fts, rowid, content, output_type)
    VALUES ('delete', old.rowid, COALESCE(old.content,''), COALESCE(old.output_type,''));
    INSERT INTO outputs_fts(rowid, content, output_type)
    VALUES (new.rowid, COALESCE(new.content,''), COALESCE(new.output_type,''));
END;

CREATE TRIGGER IF NOT EXISTS outputs_fts_delete
    AFTER DELETE ON outputs BEGIN
    INSERT INTO outputs_fts(outputs_fts, rowid, content, output_type)
    VALUES ('delete', old.rowid, COALESCE(old.content,''), COALESCE(old.output_type,''));
END;

-- Step 2: recreate model_quality_scores with focus_run_id (was path_run_id)
CREATE TABLE IF NOT EXISTS model_quality_scores_new (
    id              TEXT PRIMARY KEY,
    model_id        TEXT NOT NULL,
    task_type       TEXT NOT NULL,
    quality_score   REAL NOT NULL,
    signal_validity TEXT NOT NULL CHECK (signal_validity IN ('valid','partial')),
    focus_run_id    TEXT NOT NULL REFERENCES focus_runs(id),
    recorded_at     TEXT NOT NULL,
    extra_metadata  TEXT NOT NULL DEFAULT '{}'
);

INSERT INTO model_quality_scores_new
    (id, model_id, task_type, quality_score, signal_validity,
     focus_run_id, recorded_at, extra_metadata)
SELECT
    id, model_id, task_type, quality_score, signal_validity,
    path_run_id, recorded_at, extra_metadata
FROM model_quality_scores;

DROP TABLE IF EXISTS model_quality_scores;
ALTER TABLE model_quality_scores_new RENAME TO model_quality_scores;

CREATE INDEX IF NOT EXISTS idx_quality_scores_lookup
    ON model_quality_scores (model_id, task_type, recorded_at DESC);

-- Step 3: recreate drift_observations with focus_run_id (was path_run_id)
CREATE TABLE IF NOT EXISTS drift_observations_new (
    id              TEXT PRIMARY KEY,
    model_id        TEXT NOT NULL,
    task_type       TEXT NOT NULL,
    focus_run_id    TEXT NOT NULL REFERENCES focus_runs(id),
    drift_detected  INTEGER NOT NULL DEFAULT 0,
    drift_magnitude REAL,
    observed_at     TEXT NOT NULL,
    extra_metadata  TEXT NOT NULL DEFAULT '{}'
);

INSERT INTO drift_observations_new
    (id, model_id, task_type, focus_run_id,
     drift_detected, drift_magnitude, observed_at, extra_metadata)
SELECT
    id, model_id, task_type, path_run_id,
    drift_detected, drift_magnitude, observed_at, extra_metadata
FROM drift_observations;

DROP TABLE IF EXISTS drift_observations;
ALTER TABLE drift_observations_new RENAME TO drift_observations;

CREATE INDEX IF NOT EXISTS idx_drift_observations_lookup
    ON drift_observations (model_id, task_type, observed_at DESC);

INSERT OR IGNORE INTO schema_version (version, applied_at, description)
VALUES (5, datetime('now'),
    'Rename path_run_id to focus_run_id in outputs, model_quality_scores, drift_observations');
