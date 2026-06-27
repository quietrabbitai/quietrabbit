-- src-tauri/schema/outputs_008.sql
-- Migration 8: add extract_confirm_candidates table (item 20).
-- Stores extraction candidates surfaced by the post-execute extraction pass
-- for user confirmation before personal.db write.
-- Lives in outputs.db (per-user, per-persona, SQLCipher encrypted).
--
-- Lifecycle:
--   extract_candidates() persists candidates here after the execute() step.
--   Frontend is notified via extract_confirm_request push event.
--   User confirms or declines each candidate via submit_extract_confirm.
--   Confirmed fields are written to personal.db via save_personal_field.
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
--   when status='confirmed'. Enforced at the command layer (submit_extract_confirm),
--   not by CHECK constraint here.
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
--   is deleted. No orphan rows.
--
-- Executed within run_migrations SAVEPOINT -- atomic or rolled back.

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
VALUES (8, datetime('now'),
    'Add extract_confirm_candidates table for extract-and-confirm flow (item 20)');
