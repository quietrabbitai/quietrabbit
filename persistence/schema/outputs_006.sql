-- persistence/schema/outputs_006.sql
-- Migration 6: add consent_decisions table (D6-352).
-- Stores Gate 3 and floor consent decisions per focus run.
-- Lives in outputs.db (per-user, per-persona, SQLCipher encrypted).
--
-- decision_type: 'gate3' for cross-tier promotion consent (Gate 3);
--   'floor' for floor abstraction clamping consent.
-- decision: 'approved'|'declined' for gate3; 'proceed'|'cancel' for floor.
-- abstraction_tier: required for floor decisions; NULL for gate3.
-- save_preference: 1 if user chose to save floor consent as standing preference
--   (D5-152 -- caller writes personas.extra_metadata in shared.db separately).
--   NULL for gate3 (not applicable). 0 = explicit "do not save".
--
-- Append-only design: a run may accumulate multiple consent rows if the user
-- declines and is re-presented. No UNIQUE constraint on (focus_run_id,
-- decision_type) -- multiple decisions per type per run are permitted.
--
-- Executed within run_migrations SAVEPOINT -- atomic or rolled back.

CREATE TABLE IF NOT EXISTS consent_decisions (
    id               TEXT    PRIMARY KEY,
    focus_run_id     TEXT    NOT NULL REFERENCES focus_runs(id),
    decision_type    TEXT    NOT NULL CHECK (decision_type IN ('gate3','floor')),
    decision         TEXT    NOT NULL,
    abstraction_tier INTEGER,
    save_preference  INTEGER,
    created_at       TEXT    NOT NULL,

    CHECK (
        (decision_type = 'gate3'
            AND decision IN ('approved','declined')
            AND abstraction_tier IS NULL)
     OR (decision_type = 'floor'
            AND decision IN ('proceed','cancel')
            AND abstraction_tier IS NOT NULL
            AND abstraction_tier BETWEEN 1 AND 3)
    )
);

CREATE INDEX IF NOT EXISTS idx_consent_decisions_run
    ON consent_decisions (focus_run_id, created_at DESC);

INSERT OR IGNORE INTO schema_version (version, applied_at, description)
VALUES (6, datetime('now'),
    'Add consent_decisions table for Gate 3 and floor consent (D6-352)');
