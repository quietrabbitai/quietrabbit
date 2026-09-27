-- outputs_011.sql
--
-- items.id=496: focus_runs.status gains 'awaiting_schedule' -- the state a
-- run parks in when execute() reaches a step whose composition-authored
-- schedule_trigger (decisions.id=712 anchor_field+offset vocabulary,
-- placed per-composition-row per items.id=496's Q2 resolution) has not yet
-- fired. Distinct from 'paused' (a person paused this run) and
-- 'awaiting_user' (a person's input is needed) -- this state means nothing
-- is needed from anyone, the run is simply waiting on a clock. Kept as its
-- own value rather than reusing 'paused', following this table's own
-- precedent (outputs_009.sql's failed/discarded split) for not collapsing
-- two different "why aren't we running" reasons into one value an
-- Active Board or run-history view could no longer tell apart.
--
-- rehydrate_focus_run()/resume_run need no new branching for this --
-- items.id=245 already made every non-terminal status resume through one
-- generic handler, driven entirely by the run's own persisted current_step/
-- checkpoint, not by which status string got it there. The scheduled-step
-- sweep (conductor::scheduled_sweep, items.id=496) is what calls
-- resume_run's own path (rehydrate_focus_run + resume_execution) once the
-- trigger's anchor+offset has passed -- not a bypass of FocusRun.
--
-- WHY A TABLE REBUILD, NOT ALTER TABLE: SQLite has no ALTER COLUMN to widen
-- a CHECK constraint in place -- same constraint shared_020.sql hit
-- rebuilding shared.db's focus_settings table. Runs inside run_pending's
-- per-version SAVEPOINT (migrations.rs), atomic with the rest of this file.
-- Every existing row's status is one of the 9 pre-existing values, so the
-- copy below is a straight passthrough, not a value-mapped one.
--
-- SCHEMA AUTHORING RULE (migrations.rs): no semicolons inside string
-- literals -- parse_statements() is not a general-purpose SQL parser.

CREATE TABLE focus_runs_new (
    id                          TEXT PRIMARY KEY,
    focus_id                    TEXT NOT NULL,
    status                      TEXT NOT NULL DEFAULT 'initializing'
                                    CHECK (status IN (
                                        'initializing','running','paused',
                                        'awaiting_user','awaiting_feedback',
                                        'awaiting_extract_confirm',
                                        'awaiting_schedule',
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
    topic_id                    TEXT,
    is_quick_ask                INTEGER NOT NULL DEFAULT 0,
    user_input                  TEXT
);

INSERT INTO focus_runs_new
    (id, focus_id, status, is_fast_lane, routing_tier_used, started_at,
     completed_at, feedback_window_expires_at, signal_validity, notes,
     extra_metadata, topic_id, is_quick_ask, user_input)
SELECT
    id, focus_id, status, is_fast_lane, routing_tier_used, started_at,
    completed_at, feedback_window_expires_at, signal_validity, notes,
    extra_metadata, topic_id, is_quick_ask, user_input
FROM focus_runs;

DROP TABLE focus_runs;

ALTER TABLE focus_runs_new RENAME TO focus_runs;

CREATE INDEX IF NOT EXISTS idx_focus_runs_status
    ON focus_runs (status, started_at DESC);

CREATE INDEX IF NOT EXISTS idx_focus_runs_topic
    ON focus_runs (topic_id, started_at DESC)
    WHERE topic_id IS NOT NULL;

INSERT OR IGNORE INTO schema_version (version, applied_at, description)
VALUES (11, datetime('now'),
    'items.id=496: focus_runs.status CHECK rebuilt to add awaiting_schedule -- the scheduled-trigger wait state, resumed via the same rehydrate_focus_run()/resume_execution() path as every other non-terminal status');
