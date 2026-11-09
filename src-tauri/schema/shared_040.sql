-- shared_040.sql
--
-- items.id=879 (decisions.id=937 points 1 and 5): the declared-inputs lint
-- (conductor::declared_inputs) rejects an external step whose template names
-- anything its customization does not declare in `declared_inputs`. Writing
-- Assistant's draft_writing row was seeded by shared_038 without the field, and
-- INSERT OR IGNORE never repairs an applied row, so a database already at 038 or
-- 039 would fail to load the Focus. This is the seed-repair path recorded in
-- decisions.id=937: a NEW data-only migration, shared_038.sql is not edited.
-- Number checked against the live schema directory on the build date: shared_039
-- was the latest. No table or column is added, so the schema-shape golden does
-- not change.
--
-- The UPDATE adds one key to the existing JSON with json_set (precedent:
-- shared_025.sql, shared_028.sql, shared_031.sql) and touches nothing else, so the
-- prompt text is not restated. The declaration is exactly the names the template
-- uses today that are not already field_requirements names: user_input,
-- voice_analysis_output and voice_profile. job_title, employer and location_city
-- are implicitly declared by the step's own field_requirements. A fresh database
-- runs shared_038 first and this file second, so it reaches the same row state.
-- Only the one customization row is touched, and only its customization and
-- updated_at columns.
--
-- CONTENT CLASS (decisions.id=862): shipped core Focus content, identical for
-- every install, no personal values.
--
-- SCHEMA AUTHORING RULE (migrations.rs): no semicolons inside string literals or
-- comments -- parse_statements() is not a general-purpose SQL parser.

UPDATE focus_block_customization
SET customization = json_set(customization, '$.declared_inputs',
        json('["user_input","voice_analysis_output","voice_profile"]')),
    updated_at = datetime('now')
WHERE id = 'writing-assistant:draft_writing:default';

INSERT OR IGNORE INTO schema_version (version, applied_at, description)
VALUES (40, datetime('now'),
    'items.id=879: declared_inputs on the Writing Assistant draft_writing customization row (seed repair for the declared-inputs LOAD lint) -- data only, no schema change');
