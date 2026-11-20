-- outputs_019.sql
--
-- items.id=888 stage 4: focus_runs.context_key, the chat a run belongs to.
--
-- A run started from a chat knows its chat (FocusRun.context_key) while it runs. A
-- RESUMED run did not: rehydrate_focus_run had no context_key, so a resumed run could
-- not read the chat's one-time latch (write_mode), its launch answers or its plan
-- approval, and its store writers failed closed. The run now records the chat at the
-- INSERT of its row (every run that has a chat, not only those that park: a crashed run
-- is demoted to paused without a park write, and the archive hook of items.id=888 needs
-- every run of the chat). The value is the chat id the chat's own messages already
-- carry. It is not user content and is not cleared at a terminal status.
--
-- NULL for a run with no chat (submit_focus_run) and for every row that predates this
-- migration: such a resumed run behaves as before (no latch read, defaults for launch
-- answers unless a question is asked, then LaunchAnswersUnavailableOnResume, no approval).
--
-- Nullable, no default, no CHECK, no index -- the same shape as outputs_018.sql.
-- items.id=633 must absorb this file with the other outputs files.
--
-- SCHEMA AUTHORING RULE (migrations.rs): no semicolons inside string literals or
-- comments -- parse_statements() is not a general-purpose SQL parser.

ALTER TABLE focus_runs ADD COLUMN context_key TEXT;

INSERT OR IGNORE INTO schema_version (version, applied_at, description)
VALUES (19, datetime('now'),
    'items.id=888: focus_runs.context_key -- the chat a run belongs to, so a resumed run reads its latch, launch answers and plan approval');
