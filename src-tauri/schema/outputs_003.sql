-- persistence/schema/outputs_003.sql
-- Phase B: Plan Architecture data model extension per ADR-013.
-- Adds to outputs.db (per-user, per-life, encrypted):
--   topics                     — named persistent pursuits within a focus
--   run_history                — metadata index for all focus runs (90-day window)
--   classification_preferences — per-focus per-user sensitivity choices
--   topic_storage_locations    — authoritative registry for plan_state.db paths
--   focus_runs columns:        topic_id (nullable FK), is_quick_ask (boolean)
-- Part of Phase B data model extension (D6-226+).
--
-- Terminology: all locked terms used throughout (life_id, focus_id,
-- topic_id, action_id). ADR-013 Section 7.2 stale terms not used.
--
-- Boot Check reads topic_storage_locations — never walks filesystem.
-- outputs.db is the authoritative registry for all child databases.
--
-- Migration ordering: topics created before focus_runs is altered to
-- reference it — avoids deferred FK resolution ambiguity in SQLite.

-- Step 1: topics table.
-- Must precede ALTER TABLE focus_runs which adds topic_id FK.
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
    life_id             TEXT NOT NULL,
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

CREATE INDEX IF NOT EXISTS idx_topics_life
    ON topics (life_id, lifecycle_state, updated_at DESC);

-- Step 2: add topic_id and is_quick_ask columns to focus_runs.
-- topic_id: nullable — unnamed runs and Quick Ask runs always NULL.
-- is_quick_ask: immutable after Phase 1 LOAD. Enforced at application layer.
--   Quick Ask lifecycle termination invariant: may only terminate as
--   complete or cancelled. Never paused, never awaiting.
-- topics table created above before this ALTER to avoid FK ordering ambiguity.
ALTER TABLE focus_runs ADD COLUMN topic_id TEXT REFERENCES topics(id);
ALTER TABLE focus_runs ADD COLUMN is_quick_ask INTEGER NOT NULL DEFAULT 0;

-- Step 3: run_history table.
-- Metadata index for all focus runs — named and unnamed.
-- No conversation content stored — metadata only.
-- output_id: nullable — set to NULL if Library output deleted.
--   Entry retained for audit unless user explicitly purges.
-- promote_window_expires_at: 90-day window to promote unnamed run to topic.
--   INVARIANT: must be NULL for all rows where is_quick_ask = 1.
--   Quick Ask runs can never be promoted — enforced at application layer
--   (topic_store.py create_run_history_entry).
--   Future UI must not expose promotion option when is_quick_ask = 1.
-- Retention: 90 days default, user-configurable per ADR-013 Section 7.4.
CREATE TABLE IF NOT EXISTS run_history (
    id                          TEXT PRIMARY KEY,
    focus_run_id                TEXT NOT NULL REFERENCES focus_runs(id),
    focus_id                    TEXT NOT NULL,
    life_id                     TEXT NOT NULL,
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

-- Step 4: classification_preferences table.
-- Per-focus per-user sensitivity classification choices.
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
    life_id             TEXT NOT NULL,
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
    UNIQUE (focus_id, life_id, content_type)
);

CREATE INDEX IF NOT EXISTS idx_classification_prefs_lookup
    ON classification_preferences (focus_id, life_id, content_type);

-- Step 5: topic_storage_locations table.
-- Authoritative registry for plan_state.db file paths.
-- Boot Check reads this table — NEVER walks the filesystem.
-- outputs.db is the authoritative registry for all child databases.
-- db_path: absolute path to plan_state.db for this topic.
--   Format: /users/{user_id}/lives/{life_id}/focuses/{focus_id}/topics/{topic_id}/plan_state.db
-- verified_at: last time Boot Check confirmed the file exists and opened cleanly.
-- orphaned: set 1 if file missing or unreadable at Boot Check.
--   Orphaned topics surface as a Life dashboard notification.
--   Boot Check never auto-deletes — user action required.
CREATE TABLE IF NOT EXISTS topic_storage_locations (
    topic_id        TEXT PRIMARY KEY REFERENCES topics(id) ON DELETE CASCADE,
    db_path         TEXT NOT NULL,
    created_at      TEXT NOT NULL,
    verified_at     TEXT,
    orphaned        INTEGER NOT NULL DEFAULT 0
);

INSERT OR IGNORE INTO schema_version (version, applied_at, description)
VALUES (3, datetime('now'),
    'Phase B: topics, run_history, classification_preferences, topic_storage_locations; topic_id and is_quick_ask on focus_runs');
