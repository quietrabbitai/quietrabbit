-- persistence/schema/outputs_001.sql
-- Per-user, per-space outputs database schema: outputs.db
-- Encrypted with SQLCipher using user master key.
-- Path: /users/{user_id}/spaces/{space_id}/outputs.db
-- Stores path runs, outputs, snapshots, quality signals, drift observations.
-- model_hardware_scores: in models/scores.db (per-instance, not per-space).
-- model_quality_scores + drift_observations: here (per-user, encrypted).
-- Migration version: 1

CREATE TABLE IF NOT EXISTS schema_version (
    version         INTEGER PRIMARY KEY,
    applied_at      TEXT NOT NULL,
    description     TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS migration_lock (
    id          INTEGER PRIMARY KEY CHECK (id = 1),
    locked_at   TEXT,
    locked_by   TEXT
);
INSERT OR IGNORE INTO migration_lock (id) VALUES (1);

-- Path runs
CREATE TABLE IF NOT EXISTS path_runs (
    id                          TEXT PRIMARY KEY,
    path_id                     TEXT NOT NULL,
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

CREATE INDEX IF NOT EXISTS idx_path_runs_status
    ON path_runs (status, started_at DESC);

-- Outputs
-- Deletion sequence: zero content -> FTS5 update -> set status=deleted.
-- FTS5 shadow table compaction (optimize) runs in purge workflow code,
-- not enforced here — see persistence/output_store.py.
CREATE TABLE IF NOT EXISTS outputs (
    id                      TEXT PRIMARY KEY,
    path_run_id             TEXT NOT NULL REFERENCES path_runs(id),
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

CREATE INDEX IF NOT EXISTS idx_outputs_path_run
    ON outputs (path_run_id, created_at DESC);

CREATE INDEX IF NOT EXISTS idx_outputs_active
    ON outputs (status, created_at DESC)
    WHERE status = 'active';

-- FTS5 full-text search for library
CREATE VIRTUAL TABLE IF NOT EXISTS outputs_fts USING fts5(
    content,
    output_type,
    content='outputs',
    content_rowid='rowid'
);

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

-- Path run snapshots (checkpoints)
-- PersonalTrack NEVER serialized — re-fetched fresh on resume.
-- personal_context_manifest: field names + specialist versions at checkpoint time.
-- Resume compares manifest to current personal.db to detect changes.
-- purge_after: enforces retention policy on startup cleanup.
--   cancelled/complete: purge_after = created_at (immediate)
--   awaiting_feedback:  purge_after = Phase 5 completion time
--   paused/awaiting_user: no purge_after (preserve until resumed)
CREATE TABLE IF NOT EXISTS path_run_snapshots (
    id                          TEXT PRIMARY KEY,
    path_run_id                 TEXT NOT NULL REFERENCES path_runs(id)
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

CREATE INDEX IF NOT EXISTS idx_snapshots_run
    ON path_run_snapshots (path_run_id, created_at DESC);

CREATE INDEX IF NOT EXISTS idx_snapshots_purge
    ON path_run_snapshots (purge_after)
    WHERE purge_after IS NOT NULL;

-- Model quality scores (per path run, user-specific, encrypted)
-- Lives here (not instance scores.db) — user behavioral signals are personal.
-- Invalid runs never write here — enforced in persistence/output_store.py.
CREATE TABLE IF NOT EXISTS model_quality_scores (
    id              TEXT PRIMARY KEY,
    model_id        TEXT NOT NULL,
    task_type       TEXT NOT NULL,
    quality_score   REAL NOT NULL,
    signal_validity TEXT NOT NULL CHECK (signal_validity IN ('valid','partial')),
    path_run_id     TEXT NOT NULL REFERENCES path_runs(id),
    recorded_at     TEXT NOT NULL,
    extra_metadata  TEXT NOT NULL DEFAULT '{}'
);

CREATE INDEX IF NOT EXISTS idx_quality_scores_lookup
    ON model_quality_scores (model_id, task_type, recorded_at DESC);

-- Drift observations (voice profile calibration signals, user-specific, encrypted)
-- Lives here (not instance scores.db) — voice drift is personal behavioral data.
CREATE TABLE IF NOT EXISTS drift_observations (
    id              TEXT PRIMARY KEY,
    model_id        TEXT NOT NULL,
    task_type       TEXT NOT NULL,
    path_run_id     TEXT NOT NULL REFERENCES path_runs(id),
    drift_detected  INTEGER NOT NULL DEFAULT 0,
    drift_magnitude REAL,
    observed_at     TEXT NOT NULL,
    extra_metadata  TEXT NOT NULL DEFAULT '{}'
);

CREATE INDEX IF NOT EXISTS idx_drift_observations_lookup
    ON drift_observations (model_id, task_type, observed_at DESC);

INSERT OR IGNORE INTO schema_version (version, applied_at, description)
VALUES (1, datetime('now'), 'Initial outputs.db schema');
