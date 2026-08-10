-- outputs_003.sql
--
-- items.id=237: FocusInfo.last_used needs a real value (MAX(started_at) per
-- focus_id) instead of focus_settings.updated_at's edit-time proxy. See
-- persistence::output_store::get_focus_last_used / get_last_used_map.
--
-- focus_runs has no index on focus_id alone today -- idx_focus_runs_status
-- (status, started_at DESC) and idx_focus_runs_topic (topic_id, started_at
-- DESC WHERE topic_id IS NOT NULL) don't cover a `WHERE focus_id = ?` /
-- `GROUP BY focus_id` scan. Adding one so commands::persona::list_focuses/
-- get_focus_settings/update_focus_settings (persona.rs) don't force a full
-- table scan per call.
--
-- SQLite CAN add indexes without a full rebuild -- this is a pure additive,
-- non-destructive change; no existing row loses data.
--
-- SCHEMA AUTHORING RULE (migrations.rs): no semicolons inside string
-- literals -- parse_statements() is not a general-purpose SQL parser.

CREATE INDEX IF NOT EXISTS idx_focus_runs_focus_started
    ON focus_runs (focus_id, started_at DESC);

INSERT OR IGNORE INTO schema_version (version, applied_at, description)
VALUES (3, datetime('now'),
    'items.id=237: focus_runs(focus_id, started_at) index for FocusInfo.last_used');
