-- outputs_018.sql
--
-- items.id=883 (decisions.id=937 point 3): focus_runs.external_input.
--
-- focus_runs.user_input holds the blended chat window a run was started with (User and
-- Assistant turns) so a parked run can resume. An EXTERNAL step must receive the user's
-- own turns only, never a model turn, so a run also carries that user-turns-only text
-- (FocusRun.external_input), and a resumed run needs it too. It cannot be derived from
-- user_input: a model reply can contain a line that starts with "User: ", so parsing the
-- stored window would pass model text off as a user turn. It is stored as its own copy.
--
-- It is written next to user_input and cleared wherever user_input is cleared (a
-- terminal status, a one-time chat, a focus_level Focus without the current consent, and
-- cancel_focus_run). NULL for every row that predates this migration and for any row
-- written without one: rehydrate_focus_run() then renders {user_input} EMPTY at an
-- external step (fail closed) while a local step still sees the stored window.
--
-- Nullable, no default, no CHECK -- the same shape as outputs_010.sql.
--
-- SCHEMA AUTHORING RULE (migrations.rs): no semicolons inside string literals or
-- comments -- parse_statements() is not a general-purpose SQL parser.

ALTER TABLE focus_runs ADD COLUMN external_input TEXT;

INSERT OR IGNORE INTO schema_version (version, applied_at, description)
VALUES (18, datetime('now'),
    'items.id=883: focus_runs.external_input -- the user-turns-only text an external step receives, kept so a resumed run never derives it from the full window');
