-- shared_038.sql
--
-- items.id=129 (Stage 1): Writing Assistant on the DB composition path, and the
-- pc-03 mirror row. Data only: no table or column is added, so the schema-shape
-- golden does not change. Number checked against the live schema directory on the
-- build date: shared_037 was the latest.
--
-- 1. building_blocks gains pc-03 (Multi-turn elicitation-to-completion), so the
--    mirror goes from 9 rows to 10. This relaxes the shared_021 rule that only
--    catalog_status confirmed rows are seeded: pc-03 is a promotion_candidate
--    (single adopter, Writing Assistant). It is seeded as authoring-time
--    bookkeeping only. It has NO compiled-in handler, so a Focus citing it fails at
--    LOAD with "cataloged but has no compiled-in handler"
--    (conductor::block_dispatch::block_handler). pc-04, cb-04 and cb-09 stay out.
--
-- 2. One focuses row, writing-assistant, mirroring app/core_artifacts/focuses/
--    writing-assistant.focus: max_routing_tier anonymous_required (YAML
--    max_routing_tier 2 through external_access_from_routing_tier_yaml), output_type
--    general, status shipped. The .focus file stays as the shadowed no-row
--    fallback and is not retired. A parity test pins the DB-loaded Focus to the
--    parsed file field for field, plus exactly one extra block step.
--
-- 3. focus_block_compositions, in order: voice_analysis (0), draft_writing (1),
--    quality_check (2, block cb-06), voice_transform (3). The cb-06 step sits
--    BETWEEN draft_writing and voice_transform. It is advisory only: it reads
--    draft_output, records a verdict under draft_quality_verdict, and nothing reads
--    that variable. voice_transform's prompt names draft_output explicitly and never
--    uses previous_output, so its input is unchanged.
--
-- 4. focus_block_customization default rows (persona_id NULL) carrying each step's
--    JSON. guide_id writing-voice is explicit on every ordinary step, because the DB
--    path otherwise defaults to quick-ask-guide. Apostrophes in the prompts are
--    doubled.
--
-- CONTENT CLASS (decisions.id=862): these rows are shipped core Focus content. They
-- are identical for every install and hold no personal values. User-authored Focus
-- content must still go to encrypted storage, not here.
--
-- INSERT OR IGNORE never repairs a stale row. A corrected seed reaches an existing
-- install only through a new numbered data-only migration (UPDATE or delete and
-- insert), or, before the schema cutover (items.id=633), through the one-time reset
-- of existing databases that cutover performs.
--
-- SCHEMA AUTHORING RULE (migrations.rs): no semicolons inside string literals or
-- comments -- parse_statements() is not a general-purpose SQL parser.

INSERT OR IGNORE INTO building_blocks
    (stable_id, display_name, invocation_mode, created_at, updated_at)
VALUES
    ('pc-03', 'Multi-turn elicitation-to-completion', 'inline_composable', datetime('now'), datetime('now'));

INSERT OR IGNORE INTO focuses
    (focus_id, display_name, description, version, max_routing_tier, output_type,
     suggest_in_focuses, multi_source_validation, generic_title_template, status,
     created_at, updated_at)
VALUES
    ('writing-assistant', 'Writing Assistant', 'Craft polished writing for any purpose. Your voice, your context, your words — refined for clarity and impact.
',
     '1.0', 'anonymous_required', 'general', '[]', 0, 'Hidden item', 'shipped',
     datetime('now'), datetime('now'));

INSERT OR IGNORE INTO focus_block_compositions
    (id, focus_id, action_id, block_stable_id, sequence_index, created_at, updated_at)
VALUES
    ('writing-assistant:voice_analysis', 'writing-assistant', 'voice_analysis', NULL, 0, datetime('now'), datetime('now')),
    ('writing-assistant:draft_writing', 'writing-assistant', 'draft_writing', NULL, 1, datetime('now'), datetime('now')),
    ('writing-assistant:quality_check', 'writing-assistant', 'quality_check', 'cb-06', 2, datetime('now'), datetime('now')),
    ('writing-assistant:voice_transform', 'writing-assistant', 'voice_transform', NULL, 3, datetime('now'), datetime('now'));

INSERT OR IGNORE INTO focus_block_customization
    (id, composition_id, persona_id, customization, created_at, updated_at)
VALUES
    ('writing-assistant:voice_analysis:default', 'writing-assistant:voice_analysis', NULL,
     '{"display_name": "Understanding your style...", "guide_id": "writing-voice", "task_type": "quick_response", "routing_tier": 1, "step_type": "generate", "output_var": "voice_analysis_output", "prompt_template": "You are helping prepare a writing request. Analyse the user''s request and identify: the purpose, the intended audience, the desired tone, and the key points to cover.\nDo not write the draft yet. Only analyse the request.\nVoice preferences: {voice_profile} Request: {user_input}\n", "field_requirements": [{"name": "voice_profile", "scope": "recommended"}], "options_override": {"temperature": 0.3, "num_predict": 256}}',
     datetime('now'), datetime('now'));

INSERT OR IGNORE INTO focus_block_customization
    (id, composition_id, persona_id, customization, created_at, updated_at)
VALUES
    ('writing-assistant:draft_writing:default', 'writing-assistant:draft_writing', NULL,
     '{"display_name": "Writing your draft...", "guide_id": "writing-voice", "task_type": "creative_writing", "routing_tier": 2, "step_type": "generate", "output_var": "draft_output", "prompt_template": "Write a polished draft based on the following request and analysis. Adapt naturally to the user''s context without mentioning it directly. Be clear, purposeful, and well-structured.\nRequest: {user_input}\nAnalysis: {voice_analysis_output}\nContext (use naturally if present, ignore if empty):\n  Role: {job_title}\n  Organisation: {employer}\n  Location: {location_city}\n\nVoice preferences: {voice_profile}\n", "field_requirements": [{"name": "job_title", "scope": "optional"}, {"name": "employer", "scope": "optional"}, {"name": "location_city", "scope": "optional"}], "options_override": {"temperature": 0.5, "num_predict": 1024}}',
     datetime('now'), datetime('now'));

INSERT OR IGNORE INTO focus_block_customization
    (id, composition_id, persona_id, customization, created_at, updated_at)
VALUES
    ('writing-assistant:quality_check:default', 'writing-assistant:quality_check', NULL,
     '{"input_var": "draft_output", "output_var": "draft_quality_verdict", "criteria": [{"name": "draft_not_empty", "kind": "non_empty"}, {"name": "draft_min_length", "kind": "min_length", "min_length": 40}], "on_fail": "advisory"}',
     datetime('now'), datetime('now'));

INSERT OR IGNORE INTO focus_block_customization
    (id, composition_id, persona_id, customization, created_at, updated_at)
VALUES
    ('writing-assistant:voice_transform:default', 'writing-assistant:voice_transform', NULL,
     '{"display_name": "Refining your voice...", "guide_id": "writing-voice", "task_type": "quick_response", "routing_tier": 1, "step_type": "voice_transform", "output_var": "final_output", "prompt_template": "Refine the following draft to better match the user''s voice and preferences. Preserve all content and meaning. Adjust tone, phrasing, and rhythm only.\nVoice preferences: {voice_profile} Draft: {draft_output}\n", "field_requirements": [{"name": "voice_profile", "scope": "recommended"}], "options_override": {"temperature": 0.3, "num_predict": 1024}}',
     datetime('now'), datetime('now'));

INSERT OR IGNORE INTO schema_version (version, applied_at, description)
VALUES (38, datetime('now'),
    'items.id=129: Writing Assistant on the DB composition path (focuses, compositions with a cb-06 advisory step, default customization rows) and the pc-03 mirror row -- data only, no schema change');
