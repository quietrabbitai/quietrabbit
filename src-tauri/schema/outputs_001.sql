-- persistence/schema/outputs_001.sql
-- Per-user, per-persona outputs database schema: outputs.db
-- Encrypted with SQLCipher using user master key.
-- Path: /users/{user_id}/personas/{persona_id}/outputs.db
-- Stores focus runs, outputs, snapshots, topics, run history, classification
-- preferences, topic storage locations, consent decisions, extract-confirm
-- candidates, and quality/drift signals.
-- model_hardware_scores: in models/scores.db (per-instance, not per-persona).
-- model_quality_scores + drift_observations: here (per-user, encrypted).
--
-- CONSOLIDATION NOTE (items.id=169, 2026-07-24): this file replaces the prior
-- nine-migration chain: outputs_001 (initial, 'path_runs'/'space_id'),
-- outputs_002 (Phase A rename path_runs->focus_runs, path_id->focus_id,
-- D6-224/225), outputs_003 (Phase B: topics, run_history,
-- classification_preferences, topic_storage_locations; used 'life_id' --
-- ADR-013), outputs_004 (Phase C Persona migration life_id->persona_id,
-- D6-289 through D6-303), outputs_005 (rename path_run_id->focus_run_id in
-- outputs/model_quality_scores/drift_observations, closing the deferral
-- outputs_002 left open), outputs_006 (consent_decisions table, D6-352),
-- outputs_007 (PLACEHOLDER ONLY -- reserved for an element_consent CHECK
-- constraint extension on consent_decisions that was never built; the real
-- migration remains a dedicated pre-release session, per outputs_007's own
-- header. 'element_consent' is NOT a valid decision_type in this consolidated
-- file -- only 'gate3' and 'floor', matching outputs_006's constraint
-- unchanged. Do not add element_consent here without doing that deferred
-- work first), outputs_008 (extract_confirm_candidates table, item 20),
-- outputs_009 (extend focus_runs.status CHECK with
-- awaiting_extract_confirm, item 20).
-- Pre-release, zero shipped users -- consolidated directly to final naming
-- and final constraint shapes (except the deliberately-still-deferred
-- element_consent extension, preserved as a gap, not silently resolved).
-- Chat-DEV, per Chat-PM/Jason adjudication of Chat-DEV handoff id=99.

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

-- Focus runs.
-- topic_id: nullable — unnamed runs and Quick Ask runs always NULL.
-- is_quick_ask: immutable after Phase 1 LOAD. Enforced at application layer.
--   Quick Ask lifecycle termination invariant: may only terminate as
--   complete or cancelled. Never paused, never awaiting.
-- status 'awaiting_extract_confirm': post-execute extraction pass complete;
--   run parked pending user confirmation of extracted personal fields (item 20).
-- topic_id FK left unenforced at DB level here (topics table defined below,
-- after focus_runs, to avoid FK ordering ambiguity at fresh-install time --
-- matches original outputs_003.sql ordering rationale).
CREATE TABLE IF NOT EXISTS focus_runs (
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
    topic_id                    TEXT,
    is_quick_ask                INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX IF NOT EXISTS idx_focus_runs_status
    ON focus_runs (status, started_at DESC);

CREATE INDEX IF NOT EXISTS idx_focus_runs_topic
    ON focus_runs (topic_id, started_at DESC)
    WHERE topic_id IS NOT NULL;

-- Outputs
-- Deletion sequence: zero content -> FTS5 update -> set status=deleted.
-- FTS5 shadow table compaction (optimize) runs in purge workflow code,
-- not enforced here — see persistence/output_store.rs.
CREATE TABLE IF NOT EXISTS outputs (
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

CREATE INDEX IF NOT EXISTS idx_outputs_focus_run
    ON outputs (focus_run_id, created_at DESC);

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

-- Focus run snapshots (checkpoints)
-- PersonalTrack NEVER serialized — re-fetched fresh on resume.
-- personal_context_manifest: field names + specialist versions at checkpoint time.
-- Resume compares manifest to current personal.db to detect changes.
-- purge_after: enforces retention policy on startup cleanup.
--   cancelled/complete: purge_after = created_at (immediate)
--   awaiting_feedback:  purge_after = Phase 5 completion time
--   paused/awaiting_user: no purge_after (preserve until resumed)
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

CREATE INDEX IF NOT EXISTS idx_focus_run_snapshots_run
    ON focus_run_snapshots (focus_run_id, created_at DESC);

CREATE INDEX IF NOT EXISTS idx_focus_run_snapshots_purge
    ON focus_run_snapshots (purge_after)
    WHERE purge_after IS NOT NULL;

-- Model quality scores (per focus run, user-specific, encrypted)
-- Lives here (not instance scores.db) — user behavioral signals are personal.
-- Invalid runs never write here — enforced in persistence/output_store.rs.
CREATE TABLE IF NOT EXISTS model_quality_scores (
    id              TEXT PRIMARY KEY,
    model_id        TEXT NOT NULL,
    task_type       TEXT NOT NULL,
    quality_score   REAL NOT NULL,
    signal_validity TEXT NOT NULL CHECK (signal_validity IN ('valid','partial')),
    focus_run_id    TEXT NOT NULL REFERENCES focus_runs(id),
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
    focus_run_id    TEXT NOT NULL REFERENCES focus_runs(id),
    drift_detected  INTEGER NOT NULL DEFAULT 0,
    drift_magnitude REAL,
    observed_at     TEXT NOT NULL,
    extra_metadata  TEXT NOT NULL DEFAULT '{}'
);

CREATE INDEX IF NOT EXISTS idx_drift_observations_lookup
    ON drift_observations (model_id, task_type, observed_at DESC);

-- Topics — named persistent pursuits within a focus (ADR-013).
-- lifecycle_state values per ADR-013 Section 3.3:
--   active    — session in progress or ready to resume
--   paused    — user stopped with intent to return
--   awaiting  — blocked on Tier 3 external action (topic_id != null invariant)
--   complete  — user declared goal achieved (NEVER system-declared)
--   closed    — ended without completion
-- dormant_since: dashboard attribute only — inactivity flag, NEVER a lifecycle
--   transition. System never automatically transitions a topic to closed.
-- placeholder_name: generated at pause time as "{focus_name} — {date} {time}".
--   NEVER derived from user input content.
-- name: user-assigned. NULL = unnamed (paused topic awaiting naming).
CREATE TABLE IF NOT EXISTS topics (
    id                  TEXT PRIMARY KEY,
    focus_id            TEXT NOT NULL,
    user_id             TEXT NOT NULL,
    persona_id          TEXT NOT NULL,
    name                TEXT,
    placeholder_name    TEXT NOT NULL,
    lifecycle_state     TEXT NOT NULL DEFAULT 'active'
                            CHECK (lifecycle_state IN (
                                'active', 'paused', 'awaiting',
                                'complete', 'closed'
                            )),
    dormant_since       TEXT,
    created_at          TEXT NOT NULL,
    updated_at          TEXT NOT NULL,
    closed_at           TEXT,
    extra_metadata      TEXT NOT NULL DEFAULT '{}'
);

CREATE INDEX IF NOT EXISTS idx_topics_focus
    ON topics (focus_id, lifecycle_state, updated_at DESC);

CREATE INDEX IF NOT EXISTS idx_topics_persona
    ON topics (persona_id, lifecycle_state, updated_at DESC);

-- run_history — metadata index for all focus runs, named and unnamed.
-- No conversation content stored — metadata only.
-- output_id: nullable — set to NULL if Library output deleted.
--   Entry retained for audit unless user explicitly purges.
-- promote_window_expires_at: 90-day window to promote unnamed run to topic.
--   INVARIANT: must be NULL for all rows where is_quick_ask = 1.
--   Quick Ask runs can never be promoted — enforced at application layer
--   (topic_store.rs create_run_history_entry).
--   Future UI must not expose promotion option when is_quick_ask = 1.
-- Retention: 90 days default, user-configurable per ADR-013 Section 7.4.
CREATE TABLE IF NOT EXISTS run_history (
    id                          TEXT PRIMARY KEY,
    focus_run_id                TEXT NOT NULL REFERENCES focus_runs(id),
    focus_id                    TEXT NOT NULL,
    persona_id                  TEXT NOT NULL,
    topic_id                    TEXT REFERENCES topics(id),
    output_id                   TEXT,
    output_type                 TEXT,
    is_quick_ask                INTEGER NOT NULL DEFAULT 0,
    promote_window_expires_at   TEXT,
    created_at                  TEXT NOT NULL,
    extra_metadata              TEXT NOT NULL DEFAULT '{}'
);

CREATE INDEX IF NOT EXISTS idx_run_history_focus
    ON run_history (focus_id, created_at DESC);

CREATE INDEX IF NOT EXISTS idx_run_history_topic
    ON run_history (topic_id, created_at DESC)
    WHERE topic_id IS NOT NULL;

CREATE INDEX IF NOT EXISTS idx_run_history_promote
    ON run_history (promote_window_expires_at)
    WHERE promote_window_expires_at IS NOT NULL
    AND topic_id IS NULL
    AND is_quick_ask = 0;

-- classification_preferences — per-focus per-persona sensitivity choices.
-- Required for Mode 1 progressive disclosure (ADR-013 Amendment A, D6-210).
-- Mode 1 reads from this table. Mode 2 writes to this table on user response.
--
-- Two-dimension model per ADR-013 Section 6.2:
--   visibility_scope: tier_1_only | anonymous_tier2 | tier2_permitted | tier3_permitted
--   transformation:   no_generalize | generalize_ok | anonymize_ok | no_transform
--
-- sensitivity_preset: convenience shortcut for the four named presets.
--   standard  = tier2_permitted   + generalize_ok
--   sensitive  = anonymous_tier2  + anonymize_ok
--   private    = tier_1_only      + generalize_ok
--   locked     = tier_1_only      + no_generalize
--   NULL       = custom combination (Mode 3 explicit control)
--   Mode 1/2 always sets a non-null preset.
--   Mode 3 may set NULL preset with explicit visibility_scope + transformation.
--
-- user_calibrated: 0 = inferred_by_system (Mode 1 conservative default).
--   1 = user explicitly set this via Mode 2 or Mode 3 response.
-- confidence: 0.0-1.0. Below threshold triggers Mode 2 re-surface.
-- content_type: focus-specific content category (e.g. 'salary_data', 'location').
CREATE TABLE IF NOT EXISTS classification_preferences (
    id                  TEXT PRIMARY KEY,
    focus_id            TEXT NOT NULL,
    persona_id          TEXT NOT NULL,
    content_type        TEXT NOT NULL,
    visibility_scope    TEXT NOT NULL
                            CHECK (visibility_scope IN (
                                'tier_1_only', 'anonymous_tier2',
                                'tier2_permitted', 'tier3_permitted'
                            )),
    transformation      TEXT NOT NULL
                            CHECK (transformation IN (
                                'no_generalize', 'generalize_ok',
                                'anonymize_ok', 'no_transform'
                            )),
    sensitivity_preset  TEXT
                            CHECK (sensitivity_preset IS NULL OR
                                sensitivity_preset IN (
                                'standard', 'sensitive', 'private', 'locked'
                            )),
    user_calibrated     INTEGER NOT NULL DEFAULT 0,
    confidence          REAL NOT NULL DEFAULT 1.0,
    last_applied_at     TEXT,
    created_at          TEXT NOT NULL,
    updated_at          TEXT NOT NULL,
    extra_metadata      TEXT NOT NULL DEFAULT '{}',
    UNIQUE (focus_id, persona_id, content_type)
);

CREATE INDEX IF NOT EXISTS idx_classification_prefs_lookup
    ON classification_preferences (focus_id, persona_id, content_type);

-- topic_storage_locations — authoritative registry for plan_state.db file paths.
-- Boot Check reads this table — NEVER walks the filesystem.
-- outputs.db is the authoritative registry for all child databases.
-- db_path: absolute path to plan_state.db for this topic.
--   Format: /users/{user_id}/personas/{persona_id}/focuses/{focus_id}/topics/{topic_id}/plan_state.db
-- verified_at: last time Boot Check confirmed the file exists and opened cleanly.
-- orphaned: set 1 if file missing or unreadable at Boot Check.
--   Orphaned topics surface as a Persona dashboard notification.
--   Boot Check never auto-deletes — user action required.
CREATE TABLE IF NOT EXISTS topic_storage_locations (
    topic_id        TEXT PRIMARY KEY REFERENCES topics(id) ON DELETE CASCADE,
    db_path         TEXT NOT NULL,
    created_at      TEXT NOT NULL,
    verified_at     TEXT,
    orphaned        INTEGER NOT NULL DEFAULT 0
);

-- consent_decisions — Gate 3 and floor consent decisions per focus run (D6-352).
-- decision_type: 'gate3' for cross-tier promotion consent (Gate 3);
--   'floor' for floor abstraction clamping consent.
-- decision values are intentionally asymmetric -- different UX semantics:
--   gate3:  'approved'|'declined'  (approve or decline content promotion)
--   floor:  'proceed'|'cancel'     (proceed with or cancel execution)
--   Do not normalize these values across types.
-- abstraction_tier: required for floor decisions; NULL for gate3.
-- save_preference: 1 if user chose to save floor consent as standing preference
--   (D5-152 -- caller writes personas.extra_metadata in shared.db separately).
--   NULL for gate3 (not applicable). 0 = explicit "do not save".
--
-- NOTE: 'element_consent' is NOT a valid decision_type here. It was proposed
-- as a planned extension (originally slated for outputs_007) that was never
-- built -- outputs_007.sql was a placeholder only, its own header deferring
-- the real CHECK-constraint work to "a dedicated pre-release session" that
-- has not happened. write_element_consent_decisions() must continue to
-- return Err rather than attempting a write that would violate this
-- constraint, until that dedicated work is done. See items.id=169 handoff
-- for the flag to Chat-PM.
--
-- Append-only design: a run may accumulate multiple consent rows if the user
-- declines and is re-presented. No UNIQUE constraint on (focus_run_id,
-- decision_type) -- multiple decisions per type per run are permitted.
-- All readers MUST order by created_at DESC and select the newest row.
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

-- extract_confirm_candidates — extraction candidates surfaced by the
-- post-execute extraction pass for user confirmation before personal.db
-- write (item 20).
--
-- Lifecycle:
--   extract_candidates() persists candidates here after the execute() step.
--   Frontend is notified via extract_confirm_request push event.
--   User confirms or declines each candidate via submit_extract_confirm.
--   Confirmed fields are written to personal.db.
--   NOTE (items.id=169 finding, 2026-07-24): the confirmed-field write target
--   is save_personal_field() in personal_store.rs, which still queries the
--   personal_fields table. That table was retired by the entity-model
--   migration (personal.db entity_facts is now the fact store) but
--   personal_store.rs was never updated to match. This is a live mismatch,
--   flagged separately to Chat-PM -- not fixed as part of this consolidation.
--
-- Crash recovery:
--   On resume_run(), rows with status='confirmed' AND persisted_at IS NULL
--   indicate personal.db write did not complete. Replay save_personal_field()
--   for those rows. save_personal_field() MUST be idempotent and
--   transaction-safe. Replay after a crash between personal.db commit and
--   persisted_at update must produce no duplicate rows, duplicate audit
--   events, or altered timestamps.
--
-- status values:
--   'pending'   -- awaiting user decision
--   'confirmed' -- user accepted (confirmed_value holds accepted value)
--   'declined'  -- user rejected
--
-- Invariant: confirmed_at must be non-NULL when status='confirmed';
--   declined_at must be non-NULL when status='declined'. Enforced by
--   submit_extract_confirm() at the command layer, not by CHECK constraint,
--   to avoid rejecting partial state transitions in crash recovery paths.
--
-- sensitivity values mirror classify_sensitivity() output:
--   'medical' | 'financial' | 'personal'
--
-- confirmed_value: the value as accepted by the user (may differ from
--   extracted_value if user edited before confirming). Must be non-NULL
--   when status='confirmed'. Enforced at the command layer, not by CHECK
--   constraint here.
--
-- reason and confidence are surfaced to the frontend via extract_confirm_request
--   push event for display only. Not used in backend logic.
--
-- confidence: 0.0-1.0. Candidates below 0.6 are discarded pre-persist by
--   extract_candidates() and never appear in this table.
--
-- updated_at is maintained by application code, not a trigger.
--   Must be set explicitly in every UPDATE path (submit_extract_confirm,
--   crash-recovery replay).
--
-- focus_run_id CASCADE DELETE: candidates are cleaned up when the run
--   is deleted. No orphan rows -- but note PRAGMA foreign_keys is not set
--   by the migration runner, so this CASCADE only fires when the app layer
--   sets it at connection time; it is not automatic during migrations
--   themselves (see focus_runs table recreation history, preserved in
--   this file's header note on outputs_009).
CREATE TABLE IF NOT EXISTS extract_confirm_candidates (
    id              INTEGER PRIMARY KEY,
    focus_run_id    TEXT    NOT NULL REFERENCES focus_runs(id) ON DELETE CASCADE,
    field_name      TEXT    NOT NULL,
    extracted_value TEXT    NOT NULL,
    sensitivity     TEXT    NOT NULL CHECK (sensitivity IN ('medical','financial','personal')),
    reason          TEXT,
    confidence      REAL    NOT NULL CHECK (confidence >= 0.0 AND confidence <= 1.0),
    status          TEXT    NOT NULL DEFAULT 'pending'
                            CHECK (status IN ('pending','confirmed','declined')),
    confirmed_value TEXT,
    confirmed_at    TEXT,
    declined_at     TEXT,
    persisted_at    TEXT,
    created_at      TEXT    NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
    updated_at      TEXT    NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
);

CREATE INDEX IF NOT EXISTS idx_extract_confirm_candidates_run_status
    ON extract_confirm_candidates (focus_run_id, status);

INSERT OR IGNORE INTO schema_version (version, applied_at, description)
VALUES (1, datetime('now'),
    'outputs.db schema (consolidated 2026-07-24, items.id=169): focus_runs, outputs, focus_run_snapshots, model_quality_scores, drift_observations, topics, run_history, classification_preferences, topic_storage_locations, consent_decisions, extract_confirm_candidates');
