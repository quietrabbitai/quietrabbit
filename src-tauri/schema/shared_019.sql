-- shared_019.sql
--
-- items.id=494: fixes the 'claude' provider row's display_name, flagged by
-- shared_018.sql's own header as an audit item -- 'Claude.ai' is a
-- URL-shaped name, inconsistent with every other seeded provider
-- ('ChatGPT', 'Gemini', 'Mistral'). Corrected to 'Claude', confirmed by
-- Jason. Scoped to this one row only -- shared_018.sql's broader
-- display_name/id-vs-display_name audit is tracked separately, not done
-- here.
--
-- SCHEMA AUTHORING RULE (migrations.rs): no semicolons inside string
-- literals -- parse_statements() is not a general-purpose SQL parser.

UPDATE providers SET display_name = 'Claude' WHERE id = 'claude';

INSERT OR IGNORE INTO schema_version (version, applied_at, description)
VALUES (19, datetime('now'),
    'items.id=494: fix claude provider row''s display_name (was ''Claude.ai'', URL-shaped) to ''Claude''');
