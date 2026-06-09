-- persistence/schema/scores_001.sql
-- Per-instance model hardware scores: models/scores.db
-- Not encrypted — hardware performance metrics only, no personal data.
-- No user-identifying data in this database.
-- model_quality_scores and drift_observations: in outputs.db (per-user, encrypted).
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

-- Model hardware scores — one row per model_id + task_type.
-- seeded_score: 0.0 until Level 1 evaluation harness runs.
--   Default of 0.0 is intentional — prevents false-positive "good" scores
--   before hardware is actually tested.
-- hardware_factor: adjusted as evaluation data accumulates.
-- effective_score: generated — used by routing for model selection.
CREATE TABLE IF NOT EXISTS model_hardware_scores (
    id                  TEXT PRIMARY KEY,
    model_id            TEXT NOT NULL,
    task_type           TEXT NOT NULL,
    latency_ms          REAL NOT NULL DEFAULT 0.0,
    format_compliance   REAL NOT NULL DEFAULT 1.0,
    hardware_factor     REAL NOT NULL DEFAULT 1.0,
    seeded_score        REAL NOT NULL DEFAULT 0.0,
    effective_score     REAL NOT NULL GENERATED ALWAYS AS
                            (seeded_score * hardware_factor) STORED,
    sample_count        INTEGER NOT NULL DEFAULT 0,
    recorded_at         TEXT NOT NULL,
    extra_metadata      TEXT NOT NULL DEFAULT '{}',
    UNIQUE (model_id, task_type)
);

INSERT OR IGNORE INTO schema_version (version, applied_at, description)
VALUES (1, datetime('now'), 'Initial scores.db schema');
