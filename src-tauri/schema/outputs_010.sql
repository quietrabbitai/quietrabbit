-- outputs_010.sql
--
-- items.id=574 Part 1 (shared foundation, also unblocks items.id=245's
-- resume_run not_implemented arms): focus_runs.user_input.
--
-- rehydrate_focus_run() (lifecycle.rs) reconstructs a live FocusRun from a
-- persisted focus_runs row without the caller re-supplying the original
-- user turn -- FocusRun::new() takes user_input as a required constructor
-- argument (D6-342), but until now nothing persisted it: initialize()
-- writes the focus_runs row from self.user_input, never reading it back.
-- A resumed/re-entered run needs that same string for StepContext's
-- prompt-template rendering to behave identically to a fresh run.
--
-- Nullable, no default, no CHECK -- plain additive column matching
-- outputs_004.sql/outputs_008.sql's precedent for nullable additive
-- columns. NULL for every row that predates this migration and for any
-- future row written by a path that doesn't set it -- rehydrate_focus_run()
-- falls back to an empty string in that case (see its own doc comment).
--
-- SCHEMA AUTHORING RULE (migrations.rs): no semicolons inside string
-- literals -- parse_statements() is not a general-purpose SQL parser.

ALTER TABLE focus_runs ADD COLUMN user_input TEXT;

INSERT OR IGNORE INTO schema_version (version, applied_at, description)
VALUES (10, datetime('now'),
    'items.id=574 (shared foundation): focus_runs.user_input -- persists the original user turn so rehydrate_focus_run() can reconstruct a live FocusRun without the caller re-supplying it');
